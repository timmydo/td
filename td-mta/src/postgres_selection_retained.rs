//! Public custody bytes; possession is neither authorization nor a receipt.
use super::{Change, Error};
use crate::ids::{ConfigurationSelectionId, StoreEpoch};
use crate::postgres_configuration_store::Version;

/// Version byte, epoch, operation, expected generation, version and digest.
pub const RETAINED_BYTES: usize = 89;

impl Change {
    /// Persist before publication to retain the exact operation across process loss.
    /// This contains public identities and a source digest, never source or secret bytes.
    pub fn encode(self) -> [u8; RETAINED_BYTES] {
        let mut output = [0; RETAINED_BYTES];
        let expected = self.expected.to_be_bytes();
        let parts = [
            &[1u8][..],
            self.epoch.as_bytes().as_slice(),
            self.operation.as_bytes().as_slice(),
            expected.as_slice(),
            self.version.as_bytes().as_slice(),
            self.source_digest.as_slice(),
        ];
        for (destination, source) in output.iter_mut().zip(parts.into_iter().flatten()) {
            *destination = *source;
        }
        output
    }

    /// Decode untrusted custody bytes and derive their request digest internally.
    /// Changed bytes describe a different request; the stored receipt or current
    /// generation/source validation must still authorize any successful outcome.
    pub fn decode(mut bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != RETAINED_BYTES || take::<1>(&mut bytes)? != [1] {
            return Err(Error::Integrity);
        }
        let epoch = StoreEpoch::from_bytes(take(&mut bytes)?);
        let operation = ConfigurationSelectionId::from_bytes(take(&mut bytes)?);
        let expected = u64::from_be_bytes(take(&mut bytes)?);
        let version = Version::from_bytes(take(&mut bytes)?);
        let source_digest = take(&mut bytes)?;
        super::retained(epoch, operation, expected, version, source_digest)
    }
}
fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], Error> {
    let value = bytes
        .get(..N)
        .ok_or(Error::Integrity)?
        .try_into()
        .map_err(|_| Error::Integrity)?;
    *bytes = bytes.get(N..).ok_or(Error::Integrity)?;
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn change() -> Change {
        super::super::retained(
            StoreEpoch::from_bytes([1; 16]),
            ConfigurationSelectionId::from_bytes([2; 16]),
            u64::MAX - 1,
            Version::from_bytes([3; 16]),
            [4; 32],
        )
        .unwrap()
    }
    #[test]
    fn retained_identity_survives_process_encoding_at_full_generation_width() {
        let original = change();
        assert_eq!(Change::decode(&original.encode()).unwrap(), original);
    }
    #[test]
    fn malformed_custody_never_becomes_a_change() {
        let bytes = change().encode();
        for end in 0..bytes.len() {
            assert_eq!(Change::decode(&bytes[..end]), Err(Error::Integrity));
        }
        let mut extended = bytes.to_vec();
        extended.push(0);
        assert_eq!(Change::decode(&extended), Err(Error::Integrity));
        let mut wrong_version = bytes;
        wrong_version[0] = 2;
        assert_eq!(Change::decode(&wrong_version), Err(Error::Integrity));
        let mut exhausted = bytes;
        exhausted[33..41].fill(255);
        assert_eq!(Change::decode(&exhausted), Err(Error::Capacity));
    }
    #[test]
    fn altered_public_request_recomputes_digest_instead_of_reusing_a_receipt_key() {
        let original = change();
        for offset in [1, 33, 41, 57] {
            let mut bytes = original.encode();
            bytes[offset] ^= 1;
            let altered = Change::decode(&bytes).unwrap();
            assert_ne!(altered.request_digest, original.request_digest);
        }
        let mut bytes = original.encode();
        bytes[17] ^= 1;
        let other_operation = Change::decode(&bytes).unwrap();
        assert_ne!(other_operation.operation, original.operation);
        assert_eq!(other_operation.request_digest, original.request_digest);
    }
}
