#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::sync::atomic::{AtomicUsize, Ordering};
use td_crypto::{Crypto, Digest, Error as CryptoError, Provider, Sha256};
use td_mta::{
    format::{
        container::Error,
        table::{record_extent, Record, TableHeader},
        table_stream::Verifier,
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
fn header(table: Table, count: u64, payload: u64) -> [u8; 112] {
    let mut bytes = [0; 112];
    TableHeader {
        table,
        account: AccountId::from_bytes([0x33; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
        generation: 2,
        through: Sequence::from_u64(2),
        record_count: count,
        payload_bytes: payload,
    }
    .encode(&Provider, &mut bytes)
    .unwrap();
    bytes
}
fn thread(key: &[u8; 16]) -> [u8; 64] {
    let mut bytes = [0; 64];
    Record::new(Table::Threads, Sequence::from_u64(1), key, &[])
        .unwrap()
        .encode(&Provider, Sequence::from_u64(2), &mut bytes)
        .unwrap();
    bytes
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut digest = Provider.sha256().unwrap();
    digest.update(bytes).unwrap();
    digest.finish().unwrap()
}

#[test]
fn literal_tables_match_independent_manifest_whole_file_digests() {
    for (table, manifest) in [
        (
            include_str!("fixtures/format-v1/empty-table-1.hex"),
            include_str!("fixtures/format-v1/manifest.hex"),
        ),
        (
            include_str!("fixtures/format-v1/populated-blob-table.hex"),
            include_str!("fixtures/format-v1/manifest-history.hex"),
        ),
    ] {
        let bytes = hex(table);
        let manifest = hex(manifest);
        let mut verifier = Verifier::new(&Provider, &bytes[..112]).unwrap();
        for record in bytes[112..].chunks(113) {
            assert_eq!(record_extent(&record[..16]), Ok(113));
            assert_eq!(verifier.push(record).unwrap().key_bytes(), &[0x44; 16]);
        }
        let result = verifier.finish().unwrap();
        assert_eq!(result.digest().as_slice(), &manifest[112..144]);
        assert_eq!(result.header().file_bytes(), Ok(bytes.len() as u64));
    }
}

#[test]
fn framing_bounds_only_the_exact_prefix_without_claiming_integrity() {
    let bytes = hex(include_str!("fixtures/format-v1/record-blob.hex"));
    for end in 0..16 {
        assert_eq!(record_extent(&bytes[..end]), Err(FormatError::Truncated));
    }
    assert_eq!(record_extent(&bytes[..17]), Err(FormatError::TrailingBytes));
    assert_eq!(record_extent(&bytes[..16]), Ok(113));
    for (key, value, expected) in [
        (16u32, 0u32, Ok(64)),
        (1024, 65536, Ok(66608)),
        (15, 0, Err(FormatError::Limit)),
        (1025, 0, Err(FormatError::Limit)),
        (16, 65537, Err(FormatError::Limit)),
        (u32::MAX, u32::MAX, Err(FormatError::Limit)),
    ] {
        let mut prefix = [0; 16];
        prefix[..4].copy_from_slice(&key.to_le_bytes());
        prefix[4..8].copy_from_slice(&value.to_le_bytes());
        assert_eq!(record_extent(&prefix), expected);
    }
    // Sequence zero and arbitrary payload are not validated by framing.
    let mut prefix = bytes[..16].to_vec();
    prefix[8..].fill(0);
    assert_eq!(record_extent(&prefix), Ok(113));
}

#[test]
fn stream_order_is_unsigned_raw_bytes_and_previous_key_is_owned() {
    let hdr = header(Table::Threads, 3, 192);
    let keys = [[0x7f; 16], [0x80; 16], [0xff; 16]];
    let mut full = hdr.to_vec();
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    let mut buffer = [0; 64];
    for key in keys {
        buffer.copy_from_slice(&thread(&key));
        assert_eq!(verifier.push(&buffer).unwrap().key_bytes(), key);
        full.extend_from_slice(&buffer);
    }
    let result = verifier.finish().unwrap();
    assert_eq!(result.digest(), hash(&full));
    assert_eq!(result.header().record_count, 3);
    for second in [[0x80; 16], [0x7f; 16]] {
        let hdr = header(Table::Threads, 2, 128);
        let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
        verifier.push(&thread(&[0x80; 16])).unwrap();
        assert_eq!(
            verifier.push(&thread(&second)),
            Err(Error::Format(FormatError::InvalidValue))
        );
        assert_eq!(
            verifier.push(&thread(&[0xff; 16])),
            Err(Error::Format(FormatError::InvalidValue))
        );
        assert_eq!(
            verifier.finish(),
            Err(Error::Format(FormatError::InvalidValue))
        );
    }
    let hdr = header(Table::Keywords, 4, 262);
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    let mut full = hdr.to_vec();
    let mut key = [0x77; 18];
    let mut bytes = [0; 66];
    // Prefix extension, then shrinking to a greater key, then extension again.
    for word in [b"a".as_slice(), b"ab", b"b", b"ba"] {
        let length = 16 + word.len();
        key[16..length].copy_from_slice(word);
        let record =
            Record::new(Table::Keywords, Sequence::from_u64(1), &key[..length], &[]).unwrap();
        let n = record
            .encode(&Provider, Sequence::from_u64(2), &mut bytes)
            .unwrap();
        assert_eq!(
            verifier.push(&bytes[..n]).unwrap().key_bytes(),
            &key[..length]
        );
        full.extend_from_slice(&bytes[..n]);
    }
    assert_eq!(verifier.finish().unwrap().digest(), hash(&full));
}

#[test]
fn missing_extra_malformed_and_count_mismatched_records_never_finish() {
    let one = thread(&[1; 16]);
    let two = thread(&[2; 16]);
    for (count, payload, records) in [(1, 64, 0), (2, 128, 1), (1, 65, 1), (2, 192, 2)] {
        let hdr = header(Table::Threads, count, payload);
        let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
        for record in [&one, &two].into_iter().take(records) {
            verifier.push(record).unwrap();
        }
        assert_eq!(
            verifier.finish(),
            Err(Error::Format(FormatError::Truncated))
        );
    }
    let hdr = header(Table::Threads, 1, 128);
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    verifier.push(&one).unwrap();
    assert_eq!(
        verifier.push(&two),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    assert_eq!(
        verifier.finish(),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    // The header count can remain unfilled when its byte ceiling is exceeded.
    let hdr = header(Table::Threads, 2, 128);
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    verifier.push(&one).unwrap();
    assert_eq!(
        verifier.push(&[0; 65]),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    assert_eq!(
        verifier.finish(),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    for end in 0..64 {
        let hdr = header(Table::Threads, 1, 64);
        let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
        assert_eq!(
            verifier.push(&one[..end]),
            Err(Error::Format(FormatError::Truncated))
        );
        assert_eq!(
            verifier.push(&one),
            Err(Error::Format(FormatError::Truncated))
        );
        assert_eq!(
            verifier.finish(),
            Err(Error::Format(FormatError::Truncated))
        );
    }
    let hdr = header(Table::Threads, 1, 64);
    let mut bad = one;
    bad[63] ^= 1;
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    assert_eq!(verifier.push(&bad), Err(Error::Checksum));
    assert_eq!(verifier.push(&one), Err(Error::Checksum));
    assert_eq!(verifier.finish(), Err(Error::Checksum));
    let hdr = header(Table::Threads, 0, 0);
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    assert_eq!(
        verifier.push(&[]),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    assert_eq!(
        verifier.finish(),
        Err(Error::Format(FormatError::TrailingBytes))
    );
}

#[test]
fn large_counts_do_not_allocate_or_loop_and_maximum_keys_compare_completely() {
    let count = u64::MAX / 64 - 2;
    let hdr = header(Table::Threads, count, count * 64);
    assert_eq!(
        Verifier::new(&Provider, &hdr).unwrap().finish(),
        Err(Error::Format(FormatError::Truncated))
    );
    let mut key = [b'a'; 1024];
    key[..4].copy_from_slice(&1004u32.to_le_bytes());
    key[1008..].fill(0);
    let hdr = header(Table::ThreadAnchors, 2, 2144);
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    let mut buffer = [0; 1072];
    for last in [0u8, 1] {
        key[1023] = last;
        Record::new(Table::ThreadAnchors, Sequence::from_u64(1), &key, &[])
            .unwrap()
            .encode(&Provider, Sequence::from_u64(2), &mut buffer)
            .unwrap();
        verifier.push(&buffer).unwrap();
    }
    assert_eq!(verifier.finish().unwrap().header().payload_bytes, 2144);
    let hdr = header(Table::ThreadAnchors, 2, 1072);
    let mut verifier = Verifier::new(&Provider, &hdr).unwrap();
    verifier.push(&buffer).unwrap();
    // All declared bytes can be present while a declared record is missing.
    assert_eq!(
        verifier.finish(),
        Err(Error::Format(FormatError::Truncated))
    );
}

#[derive(Clone, Copy)]
enum Fault {
    Start(usize),
    Update(usize, usize),
    Finish(usize),
}
struct FaultCrypto {
    fault: Fault,
    factories: AtomicUsize,
}
struct FaultDigest {
    fault: Fault,
    index: usize,
    updates: usize,
    inner: Sha256,
    failed: bool,
}
impl Digest for FaultDigest {
    fn update(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
        self.updates += 1;
        if self.failed
            || matches!(self.fault, Fault::Update(i, u) if i == self.index && u == self.updates)
        {
            self.failed = true;
            return Err(CryptoError::Crypto);
        }
        let result = self.inner.update(bytes);
        self.failed |= result.is_err();
        result
    }
    fn finish(self) -> Result<[u8; 32], CryptoError> {
        if self.failed || matches!(self.fault, Fault::Finish(i) if i == self.index) {
            return Err(CryptoError::Crypto);
        }
        self.inner.finish()
    }
}
impl Crypto for FaultCrypto {
    type Sha256 = FaultDigest;
    type SigningKey = ();
    fn sha256(&self) -> Result<FaultDigest, CryptoError> {
        let index = self.factories.fetch_add(1, Ordering::Relaxed) + 1;
        if matches!(self.fault, Fault::Start(i) if i == index) {
            return Err(CryptoError::Crypto);
        }
        Ok(FaultDigest {
            fault: self.fault,
            index,
            updates: 0,
            inner: Sha256::try_new()?,
            failed: false,
        })
    }
    fn equal_digest(&self, a: &[u8; 32], b: &[u8; 32]) -> bool {
        Provider.equal_digest(a, b)
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

#[test]
fn provider_failures_cannot_be_skipped_or_finished_successfully() {
    let crypto = FaultCrypto {
        fault: Fault::Update(1, 1),
        factories: AtomicUsize::new(0),
    };
    let mut digest = crypto.sha256().unwrap();
    assert_eq!(digest.update(b"a"), Err(CryptoError::Crypto));
    assert_eq!(digest.update(b"b"), Err(CryptoError::Crypto));
    assert_eq!(digest.finish(), Err(CryptoError::Crypto));
    let hdr = header(Table::Threads, 1, 64);
    let record = thread(&[1; 16]);
    let error = Error::Crypto(CryptoError::Crypto);
    for fault in [
        Fault::Start(1),
        Fault::Update(1, 1),
        Fault::Finish(1),
        Fault::Start(2),
        Fault::Update(2, 1),
    ] {
        let crypto = FaultCrypto {
            fault,
            factories: AtomicUsize::new(0),
        };
        assert!(matches!(Verifier::new(&crypto, &hdr), Err(e) if e == error));
    }
    for fault in [
        Fault::Start(3),
        Fault::Update(3, 1),
        Fault::Finish(3),
        Fault::Update(2, 2),
    ] {
        let crypto = FaultCrypto {
            fault,
            factories: AtomicUsize::new(0),
        };
        let mut verifier = Verifier::new(&crypto, &hdr).unwrap();
        assert_eq!(verifier.push(&record), Err(error));
        let factories = crypto.factories.load(Ordering::Relaxed);
        assert_eq!(verifier.push(&record), Err(error));
        assert_eq!(verifier.finish(), Err(error));
        assert_eq!(crypto.factories.load(Ordering::Relaxed), factories);
    }
    let crypto = FaultCrypto {
        fault: Fault::Finish(2),
        factories: AtomicUsize::new(0),
    };
    let mut verifier = Verifier::new(&crypto, &hdr).unwrap();
    verifier.push(&record).unwrap();
    assert_eq!(verifier.finish(), Err(error));
}
