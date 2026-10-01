#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use td_crypto::{Crypto, Digest, Error as CryptoError, Provider, Sha256};
use td_mta::{
    format::{
        container::{Current, Error, JournalHeader, StoreIdentity},
        Error as FormatError, Sequence,
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
    for fault in [Fault::Start, Fault::Update, Fault::Finish] {
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

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    Start,
    Update,
    Finish,
    Compare,
}
struct FaultCrypto(Fault);
struct FaultDigest {
    fault: Fault,
    inner: Sha256,
    failed: bool,
}
impl Digest for FaultDigest {
    fn update(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
        if self.failed || self.fault == Fault::Update {
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
