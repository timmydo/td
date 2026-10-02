#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use td_crypto::{Crypto, Digest, Error as CryptoError, Provider, Sha256};
use td_mta::{
    format::{
        container::{Error, JournalHeader},
        frame::{seal, DecodeError, Frame},
        journal_stream::{Error as JournalError, Verifier},
        key::{Key, SourceKind},
        operation::{Operation, Value},
        Error as F, ObjectType, Sequence, Table, MAX_FRAME_BYTES,
    },
    ids::InstanceId,
    ports::ChangeAction,
};
fn seq(n: u64) -> Sequence {
    Sequence::from_u64(n)
}
fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut digest = Provider.sha256().unwrap();
    digest.update(bytes).unwrap();
    digest.finish().unwrap()
}
fn rehash(bytes: &mut [u8]) {
    let n = bytes.len() - 32;
    let digest = hash(&bytes[..n]);
    bytes[n..].copy_from_slice(&digest);
}
fn journal(base: u64) -> [u8; 96] {
    let literal = hex(include_str!("fixtures/format-v1/empty-journal.hex"));
    let mut header = JournalHeader::decode(&Provider, &literal).unwrap();
    header.base = seq(base);
    let mut bytes = [0; 96];
    header.encode(&Provider, &mut bytes).unwrap();
    bytes
}
fn deletes(sequence: u64, count: usize) -> Vec<u8> {
    let mut bytes = vec![0; 104 + 28 * count];
    let key = [0x44; 16];
    let op = Operation::delete(Table::Blobs, &key).unwrap();
    for output in bytes[64..64 + 28 * count].as_chunks_mut::<28>().0 {
        op.encode(output).unwrap();
    }
    seal(&Provider, seq(sequence), count, &mut bytes).unwrap();
    bytes
}
#[test]
fn literal_frames_validate_completely_and_seal_without_changing_payloads() {
    for (literal, previous, count) in [
        (include_str!("fixtures/format-v1/frame-put-blob.hex"), 0, 1),
        (
            include_str!("fixtures/format-v1/frame-delete-change.hex"),
            1,
            2,
        ),
    ] {
        let bytes = hex(literal);
        let frame = Frame::decode(&Provider, seq(previous), &bytes).unwrap();
        assert_eq!(frame.header().operations, count);
        for (ordinal, entry) in frame.operations().enumerate() {
            assert_eq!(entry.unwrap().ordinal, ordinal);
        }
        assert_eq!(frame.operations().count(), count);
        let mut output = bytes.clone();
        output[..64].fill(0xa5);
        let footer = output.len() - 40;
        output[footer..].fill(0xa5);
        assert_eq!(
            seal(&Provider, seq(previous + 1), count, &mut output).unwrap(),
            frame
        );
        assert_eq!(output, bytes);
        for n in 0..bytes.len() {
            assert_eq!(
                Frame::decode(&Provider, seq(previous), &bytes[..n]),
                Err(DecodeError::Incomplete {
                    required: if n < 64 { 64 } else { bytes.len() },
                    supplied: n
                })
            );
        }
        for offset in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[offset] ^= 1;
            assert_eq!(
                Frame::decode(&Provider, seq(previous), &bad),
                Err(DecodeError::Invalid(Error::Checksum))
            );
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(
            Frame::decode(&Provider, seq(previous), &extra),
            Err(DecodeError::Invalid(Error::Format(F::TrailingBytes)))
        );
        assert_eq!(
            Frame::decode(&Provider, seq(previous + 1), &bytes),
            Err(DecodeError::Invalid(Error::Format(F::InvalidValue)))
        );
    }
}
#[test]
fn valid_checksums_do_not_legalize_bad_payloads_footers_or_counts() {
    let original = hex(include_str!("fixtures/format-v1/frame-delete-change.hex"));
    for (offset, value, error) in [
        (64, 0, F::InvalidTag),
        (65, 1, F::InvalidTag),
        (94, 4, F::InvalidValue),
        (120, 0, F::InvalidTag),
    ] {
        let mut bytes = original.clone();
        bytes[offset] = value;
        rehash(&mut bytes);
        assert_eq!(
            Frame::decode(&Provider, seq(1), &bytes),
            Err(DecodeError::Invalid(Error::Format(error)))
        );
    }
    // The header permits one generic 56-byte operation; the payload contains two.
    let mut wrong = original.clone();
    wrong[20..24].copy_from_slice(&1u32.to_le_bytes());
    rehash(&mut wrong[..64]);
    rehash(&mut wrong);
    assert_eq!(
        Frame::decode(&Provider, seq(1), &wrong),
        Err(DecodeError::Invalid(Error::Format(F::TrailingBytes)))
    );
    let mut invalid_row = hex(include_str!("fixtures/format-v1/frame-put-blob.hex"));
    invalid_row[92] = 255;
    rehash(&mut invalid_row);
    assert_eq!(
        Frame::decode(&Provider, seq(0), &invalid_row),
        Err(DecodeError::Invalid(Error::Format(F::InvalidTag)))
    );
    let mut oversized = original.clone();
    oversized[68..72].copy_from_slice(&1024u32.to_le_bytes());
    rehash(&mut oversized);
    assert_eq!(
        Frame::decode(&Provider, seq(1), &oversized),
        Err(DecodeError::Invalid(Error::Format(F::Truncated)))
    );
    let mut invalid_header = original.clone();
    invalid_header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    rehash(&mut invalid_header[..64]);
    rehash(&mut invalid_header);
    assert_eq!(
        Frame::decode(&Provider, seq(1), &invalid_header),
        Err(DecodeError::Invalid(Error::Format(F::Limit)))
    );
}
#[test]
fn sealing_errors_preserve_every_input_byte_and_sequence_never_wraps() {
    let original = deletes(1, 2);
    for (sequence, count) in [(0, 2), (1, 0), (1, 1), (1, 3), (1, 4097)] {
        let mut bytes = original.clone();
        assert!(seal(&Provider, seq(sequence), count, &mut bytes).is_err());
        assert_eq!(bytes, original);
    }
    let mut bad = original.clone();
    bad[64] = 0;
    let before = bad.clone();
    assert!(seal(&Provider, seq(1), 2, &mut bad).is_err());
    assert_eq!(bad, before);
    let mut identity = vec![0xa5; 132];
    Operation::change(ObjectType::Identity, ChangeAction::Updated, &[0; 16])
        .encode(&mut identity[64..92])
        .unwrap();
    let before = identity.clone();
    assert_eq!(
        seal(&Provider, seq(1), 1, &mut identity),
        Err(Error::Format(F::InvalidValue))
    );
    assert_eq!(identity, before);
    let last = deletes(u64::MAX, 1);
    assert!(Frame::decode(&Provider, seq(u64::MAX - 1), &last).is_ok());
    assert_eq!(
        Frame::decode(&Provider, seq(u64::MAX), &last),
        Err(DecodeError::Invalid(Error::Format(F::Exhausted)))
    );
    let frame = Frame::decode(&Provider, seq(0), &original).unwrap();
    let entries: Vec<_> = frame.operations().map(Result::unwrap).collect();
    assert_eq!(entries[0].operation, entries[1].operation);
    assert_eq!((entries[0].ordinal, entries[1].ordinal), (0, 1));
    assert!(matches!(entries[0].operation.value(), Value::Row(_)));
}
fn maximum(sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; MAX_FRAME_BYTES];
    let object = [b'x'; 998];
    let mut key = [0; 1024];
    let n = Key::Import {
        instance: InstanceId::from_bytes([1; 16]),
        kind: SourceKind::Email,
        account: b"a",
        object: &object,
    }
    .encode(&mut key)
    .unwrap();
    assert_eq!(n, 1024);
    let operation = Operation::delete(Table::Imports, &key).unwrap();
    let mut offset = 64;
    for _ in 0..1012 {
        offset += operation.encode(&mut bytes[offset..]).unwrap();
    }
    let n = Key::Import {
        instance: InstanceId::from_bytes([1; 16]),
        kind: SourceKind::Email,
        account: b"a",
        object: b"bc",
    }
    .encode(&mut key)
    .unwrap();
    offset += Operation::delete(Table::Imports, &key[..n])
        .unwrap()
        .encode(&mut bytes[offset..])
        .unwrap();
    assert_eq!(offset, bytes.len() - 40);
    seal(&Provider, seq(sequence), 1013, &mut bytes).unwrap();
    bytes
}
#[test]
fn exact_frame_and_journal_byte_and_operation_caps_are_independent() {
    let mut stream = Verifier::new(&Provider, &journal(0)).unwrap();
    for sequence in 1..=4 {
        let bytes = maximum(sequence);
        let frame = stream.push(&bytes).unwrap();
        assert_eq!(frame.operations().count(), 1013);
    }
    assert_eq!(
        stream.push(&deletes(5, 1)),
        Err(JournalError::Journal(Error::Format(F::Limit)))
    );
    assert_eq!(
        stream.finish(),
        Err(JournalError::Journal(Error::Format(F::Limit)))
    );
    let max = maximum(1);
    let mut too_large = max.clone();
    too_large.push(0);
    let before = too_large.clone();
    assert_eq!(
        seal(&Provider, seq(1), 1013, &mut too_large),
        Err(Error::Format(F::Limit))
    );
    assert_eq!(too_large, before);
    let mut stream = Verifier::new(&Provider, &journal(0)).unwrap();
    for sequence in 1..=2 {
        let bytes = deletes(sequence, 4096);
        assert_eq!(stream.push(&bytes).unwrap().operations().count(), 4096);
    }
    assert_eq!(
        stream.push(&deletes(3, 1)),
        Err(JournalError::Journal(Error::Format(F::Limit)))
    );
    assert_eq!(
        stream.finish(),
        Err(JournalError::Journal(Error::Format(F::Limit)))
    );
    let bytes = deletes(1, 4096);
    assert!(Frame::decode(&Provider, seq(0), &bytes).is_ok());
}
#[test]
fn journal_summary_hashes_exact_supplied_frames_and_failure_is_sticky() {
    let header = journal(0);
    let first = hex(include_str!("fixtures/format-v1/frame-put-blob.hex"));
    let second = hex(include_str!("fixtures/format-v1/frame-delete-change.hex"));
    let mut stream = Verifier::new(&Provider, &header).unwrap();
    stream.push(&first).unwrap();
    stream.push(&second).unwrap();
    let summary = stream.finish().unwrap();
    assert_eq!(
        summary.header(),
        JournalHeader::decode(&Provider, &header).unwrap()
    );
    assert_eq!(summary.through(), seq(2));
    assert_eq!(summary.operations(), 3);
    assert_eq!(summary.frame_bytes(), first.len() + second.len());
    assert_eq!(summary.file_bytes(), Ok(96 + first.len() + second.len()));
    assert_eq!(
        summary.digest(),
        hash(&[header.as_slice(), &first, &second].concat())
    );
    for base in [0, u64::MAX] {
        let header = journal(base);
        let empty = Verifier::new(&Provider, &header).unwrap().finish().unwrap();
        assert_eq!(empty.through(), seq(base));
        assert_eq!(empty.operations(), 0);
        assert_eq!(empty.file_bytes(), Ok(96));
        assert_eq!(empty.digest(), hash(&header));
    }
    for invalid in [&second[..], &first[..first.len() - 1]] {
        let mut stream = Verifier::new(&Provider, &header).unwrap();
        let error = stream.push(invalid).unwrap_err();
        assert_eq!(stream.push(&first), Err(error));
        assert_eq!(stream.finish(), Err(error));
    }
    let mut corrupt = first.clone();
    corrupt[70] ^= 1;
    let mut stream = Verifier::new(&Provider, &header).unwrap();
    let error = stream.push(&corrupt).unwrap_err();
    assert_eq!(
        error,
        JournalError::Frame(DecodeError::Invalid(Error::Checksum))
    );
    assert_eq!(stream.push(&first), Err(error));
    assert_eq!(stream.finish(), Err(error));
    let mut stream = Verifier::new(&Provider, &journal(u64::MAX)).unwrap();
    assert_eq!(
        stream.push(&first),
        Err(JournalError::Journal(Error::Format(F::Exhausted)))
    );
}

struct FaultState {
    step: AtomicUsize,
    at: usize,
}
impl FaultState {
    fn check(&self) -> Result<(), CryptoError> {
        let next = self.step.fetch_add(1, Ordering::Relaxed) + 1;
        if next == self.at {
            Err(CryptoError::Crypto)
        } else {
            Ok(())
        }
    }
}
struct FaultCrypto(Arc<FaultState>);
impl FaultCrypto {
    fn new(at: usize) -> Self {
        Self(Arc::new(FaultState {
            step: AtomicUsize::new(0),
            at,
        }))
    }
}
struct FaultDigest {
    state: Arc<FaultState>,
    inner: Sha256,
}
impl Digest for FaultDigest {
    fn update(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
        self.state.check()?;
        self.inner.update(bytes)
    }
    fn finish(self) -> Result<[u8; 32], CryptoError> {
        self.state.check()?;
        self.inner.finish()
    }
}
impl Crypto for FaultCrypto {
    type Sha256 = FaultDigest;
    type SigningKey = ();
    fn sha256(&self) -> Result<FaultDigest, CryptoError> {
        self.0.check()?;
        Ok(FaultDigest {
            state: self.0.clone(),
            inner: Sha256::try_new()?,
        })
    }
    fn equal_digest(&self, a: &[u8; 32], b: &[u8; 32]) -> bool {
        self.0.check().is_ok() && Provider.equal_digest(a, b)
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
fn failures_in_both_hash_phases_preserve_outputs_and_poison_journal_completion() {
    let original = deletes(1, 1);
    for at in 1..=8 {
        let mut output = original.clone();
        assert!(seal(&FaultCrypto::new(at), seq(1), 1, &mut output).is_err());
        assert_eq!(output, original);
        assert!(Frame::decode(&FaultCrypto::new(at), seq(0), &original).is_err());
    }
    assert!(Frame::decode(&FaultCrypto::new(9), seq(0), &original).is_ok());
    let header = journal(0);
    for at in 1..=6 {
        assert!(Verifier::new(&FaultCrypto::new(at), &header).is_err());
    }
    for at in 7..=15 {
        let crypto = FaultCrypto::new(at);
        let mut stream = Verifier::new(&crypto, &header).unwrap();
        let error = stream.push(&original).unwrap_err();
        assert_eq!(stream.push(&original), Err(error));
        assert_eq!(stream.finish(), Err(error));
    }
    let crypto = FaultCrypto::new(16);
    let mut stream = Verifier::new(&crypto, &header).unwrap();
    stream.push(&original).unwrap();
    assert_eq!(
        stream.finish(),
        Err(JournalError::Journal(Error::Crypto(CryptoError::Crypto)))
    );
}

#[test]
fn a_complete_short_row_is_corruption_even_when_its_nested_error_is_truncated() {
    let mut bytes = deletes(1, 1);
    bytes[64] = 1; // Complete PUT envelope with an absent blob row.
    rehash(&mut bytes);
    assert_eq!(
        Frame::decode(&Provider, seq(0), &bytes),
        Err(DecodeError::Invalid(Error::Format(F::Truncated)))
    );
    assert_eq!(
        Frame::decode(&Provider, seq(0), &bytes[..131]),
        Err(DecodeError::Incomplete {
            required: 132,
            supplied: 131
        })
    );
    assert_eq!(
        Frame::decode(&Provider, seq(1), &bytes[..64]),
        Err(DecodeError::Invalid(Error::Format(F::InvalidValue)))
    );
    bytes[32] ^= 1;
    assert_eq!(
        Frame::decode(&Provider, seq(0), &bytes[..64]),
        Err(DecodeError::Invalid(Error::Checksum))
    );
}

#[test]
fn impossible_partial_journal_tails_are_refused_before_incomplete_classification() {
    let limit = Err(JournalError::Journal(Error::Format(F::Limit)));
    let header = journal(0);
    let mut byte_limited = Verifier::new(&Provider, &header).unwrap();
    for sequence in 1..=3 {
        byte_limited.push(&maximum(sequence)).unwrap();
    }
    byte_limited.push(&deletes(4, 1)).unwrap();
    let next = maximum(5);
    assert_eq!(byte_limited.push(&next[..74]), limit);
    assert_eq!(
        byte_limited.finish(),
        Err(JournalError::Journal(Error::Format(F::Limit)))
    );
    let mut operation_limited = Verifier::new(&Provider, &header).unwrap();
    operation_limited.push(&deletes(1, 4096)).unwrap();
    operation_limited.push(&deletes(2, 4095)).unwrap();
    let next = deletes(3, 2);
    assert_eq!(operation_limited.push(&next[..64]), limit);
    assert_eq!(
        operation_limited.finish(),
        Err(JournalError::Journal(Error::Format(F::Limit)))
    );
    let mut full = Verifier::new(&Provider, &header).unwrap();
    full.push(&deletes(1, 4096)).unwrap();
    full.push(&deletes(2, 4096)).unwrap();
    assert_eq!(full.push(&[0]), limit);
    let mut exhausted = Verifier::new(&Provider, &journal(u64::MAX)).unwrap();
    assert_eq!(
        exhausted.push(&[0]),
        Err(JournalError::Journal(Error::Format(F::Exhausted)))
    );
    let mut possible = Verifier::new(&Provider, &header).unwrap();
    assert_eq!(
        possible.push(&deletes(1, 1)[..64]),
        Err(JournalError::Frame(DecodeError::Incomplete {
            required: 132,
            supplied: 64
        }))
    );
}
#[test]
fn diagnostics_distinguish_journal_and_frame_failures() {
    let mut header = journal(0);
    header[0] ^= 1;
    let error = Verifier::new(&Provider, &header).err().unwrap();
    assert_eq!(
        error.to_string(),
        "journal validation failed: container checksum mismatch"
    );
    let mut frame = deletes(1, 1);
    frame[64] ^= 1;
    let mut stream = Verifier::new(&Provider, &journal(0)).unwrap();
    let error = stream.push(&frame).unwrap_err();
    assert_eq!(
        error.to_string(),
        "frame validation failed: container checksum mismatch"
    );
}

fn streamed(bytes: &[u8], previous: u64) -> Result<td_mta::format::frame_stream::Summary, Error> {
    use td_mta::format::{frame_stream, operation};
    let mut stream = frame_stream::Verifier::new(&Provider, seq(previous), &bytes[..64])?;
    let end = bytes.len() - 40;
    let mut offset = 64;
    while offset < end {
        let prefix = bytes.get(offset..offset + 12).ok_or(F::Truncated)?;
        let length = operation::extent(prefix)?;
        stream.push(bytes.get(offset..offset + length).ok_or(F::Truncated)?)?;
        offset += length;
    }
    stream.finish(&bytes[end..])
}

#[test]
fn incremental_frames_match_literals_and_maximum_complete_frames() {
    use td_mta::format::frame_stream;
    for (bytes, previous) in [
        (
            hex(include_str!("fixtures/format-v1/frame-put-blob.hex")),
            0,
        ),
        (
            hex(include_str!("fixtures/format-v1/frame-delete-change.hex")),
            1,
        ),
        (maximum(1), 0),
        (deletes(1, 4096), 0),
        (deletes(u64::MAX, 2), u64::MAX - 1),
    ] {
        let frame = Frame::decode(&Provider, seq(previous), &bytes).unwrap();
        let mut verifier =
            frame_stream::Verifier::new(&Provider, seq(previous), &bytes[..64]).unwrap();
        assert_eq!(verifier.header(), frame.header());
        // Reusing one operation buffer proves the verifier does not retain entries.
        let mut scratch = vec![0; 66572];
        for entry in frame.operations() {
            let entry = entry.unwrap();
            let n = entry.operation.encode(&mut scratch).unwrap();
            assert_eq!(verifier.push(&scratch[..n]).unwrap(), entry);
        }
        let summary = verifier.finish(&bytes[bytes.len() - 40..]).unwrap();
        assert_eq!(summary.header(), frame.header());
        assert_eq!(summary.digest().as_slice(), &bytes[bytes.len() - 32..]);
        assert_eq!(streamed(&bytes, previous), Ok(summary));
    }
    assert!(std::mem::size_of::<frame_stream::Verifier<'_, Provider>>() <= 512);
    const {
        assert!(
            td_mta::format::OPERATION_HEADER_BYTES
                + td_mta::format::MAX_KEY_BYTES
                + td_mta::format::MAX_VALUE_BYTES
                <= td_mta::format::table::MAX_RECORD_BYTES
        );
    }
}

#[test]
fn incremental_frame_errors_are_terminal_and_cannot_complete_partial_data() {
    use td_mta::format::{frame_header::Header, frame_stream::Verifier as Stream};
    let bytes = deletes(1, 2);
    let header = &bytes[..64];
    let operation = &bytes[64..92];
    let footer = &bytes[120..];
    for n in 0..64 {
        assert!(Stream::new(&Provider, seq(0), &header[..n]).is_err());
    }
    for previous in [1, u64::MAX] {
        assert!(Stream::new(&Provider, seq(previous), header).is_err());
    }
    assert!(Stream::new(&Provider, seq(0), &bytes[..65]).is_err());
    for count in 0..2 {
        let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
        for _ in 0..count {
            stream.push(operation).unwrap();
        }
        assert_eq!(stream.finish(footer), Err(Error::Format(F::Truncated)));
    }
    for n in 0..28 {
        let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
        let error = stream.push(&operation[..n]).unwrap_err();
        assert_eq!(stream.push(operation), Err(error));
        assert_eq!(stream.finish(footer), Err(error));
    }
    for n in 0..40 {
        let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
        stream.push(operation).unwrap();
        stream.push(operation).unwrap();
        assert_eq!(
            stream.finish(&footer[..n]),
            Err(Error::Format(F::Truncated))
        );
    }
    let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
    stream.push(operation).unwrap();
    stream.push(operation).unwrap();
    assert_eq!(stream.push(operation), Err(Error::Format(F::TrailingBytes)));
    assert_eq!(stream.finish(footer), Err(Error::Format(F::TrailingBytes)));
    let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
    stream.push(operation).unwrap();
    stream.push(operation).unwrap();
    let mut extra_footer = footer.to_vec();
    extra_footer.push(0);
    assert_eq!(
        stream.finish(&extra_footer),
        Err(Error::Format(F::TrailingBytes))
    );
    // Exceed bytes while the declared operation count still has room.
    let mut larger = [0; 29];
    Operation::delete(Table::Keywords, &[b'a'; 17])
        .unwrap()
        .encode(&mut larger)
        .unwrap();
    let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
    stream.push(operation).unwrap();
    assert_eq!(stream.push(&larger), Err(Error::Format(F::TrailingBytes)));
    assert_eq!(stream.push(operation), Err(Error::Format(F::TrailingBytes)));
    assert_eq!(stream.finish(footer), Err(Error::Format(F::TrailingBytes)));
    // Independently exercise count and payload mismatches with valid headers.
    for (count, payload, pushed, expected) in [
        (1, 56, 2, F::TrailingBytes),
        (2, 56, 3, F::TrailingBytes),
        (2, 57, 2, F::Truncated),
    ] {
        let mut hdr = [0; 64];
        Header {
            frame_bytes: 104 + payload,
            operations: count,
            sequence: seq(1),
        }
        .encode(&Provider, &mut hdr)
        .unwrap();
        let mut stream = Stream::new(&Provider, seq(0), &hdr).unwrap();
        for _ in 0..pushed {
            let _ = stream.push(operation);
        }
        assert_eq!(stream.finish(footer), Err(Error::Format(expected)));
    }
    // Exact payload bytes cannot excuse a missing declared operation.
    let mut wrong_count = vec![0; 160];
    Operation::delete(Table::Keywords, &[b'a'; 44])
        .unwrap()
        .encode(&mut wrong_count[64..120])
        .unwrap();
    seal(&Provider, seq(1), 1, &mut wrong_count).unwrap();
    Header {
        frame_bytes: 160,
        operations: 2,
        sequence: seq(1),
    }
    .encode(&Provider, &mut wrong_count[..64])
    .unwrap();
    rehash(&mut wrong_count);
    assert_eq!(
        Frame::decode(&Provider, seq(0), &wrong_count),
        Err(DecodeError::Invalid(Error::Format(F::Truncated)))
    );
    assert_eq!(streamed(&wrong_count, 0), Err(Error::Format(F::Truncated)));
    // Identity CHANGE is legal in the operation codec but forbidden in a frame.
    let mut identity = [0; 28];
    Operation::change(ObjectType::Identity, ChangeAction::Updated, &[1; 16])
        .encode(&mut identity)
        .unwrap();
    assert!(Operation::decode(&identity).is_ok());
    let mut stream = Stream::new(&Provider, seq(0), header).unwrap();
    assert_eq!(stream.push(&identity), Err(Error::Format(F::InvalidValue)));
    assert_eq!(stream.push(operation), Err(Error::Format(F::InvalidValue)));
    assert_eq!(stream.finish(footer), Err(Error::Format(F::InvalidValue)));
}

#[test]
fn incremental_frame_integrity_and_provider_failures_refuse_completion() {
    use td_mta::format::frame_stream::Verifier as Stream;
    let bytes = deletes(1, 1);
    for offset in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[offset] ^= 1;
        assert!(streamed(&bad, 0).is_err(), "accepted corrupt byte {offset}");
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(streamed(&extra, 0).is_err());
    // Existing fault provider visits factory, update, finish and equality calls.
    for at in 1..=10 {
        let crypto = FaultCrypto::new(at);
        match Stream::new(&crypto, seq(0), &bytes[..64]) {
            Err(_) => assert!(at <= 6),
            Ok(mut stream) => match stream.push(&bytes[64..92]) {
                Err(error) => {
                    assert_eq!(at, 7);
                    let steps = crypto.0.step.load(Ordering::Relaxed);
                    assert_eq!(stream.push(&bytes[64..92]), Err(error));
                    assert_eq!(stream.finish(&bytes[92..]), Err(error));
                    assert_eq!(crypto.0.step.load(Ordering::Relaxed), steps);
                }
                Ok(_) => assert!(stream.finish(&bytes[92..]).is_err()),
            },
        }
    }
    let crypto = FaultCrypto::new(11);
    let mut stream = Stream::new(&crypto, seq(0), &bytes[..64]).unwrap();
    stream.push(&bytes[64..92]).unwrap();
    assert!(stream.finish(&bytes[92..]).is_ok());
}
