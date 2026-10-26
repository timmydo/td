//! Backend-accelerated SHA-256 for bounded bulk I/O workers.
use crate::{Digest, Error};
use aws_lc_rs::digest::{Context, SHA256};
use std::panic::{catch_unwind, UnwindSafe};

const MAX_INPUT_BYTES: u64 = u64::MAX / 8;

/// Streaming bulk digest with opaque native storage. Unlike `Sha256`, this
/// operation may allocate. Callers bound update sizes and enforce deadlines.
pub struct BulkSha256 {
    context: Option<Context>,
    bytes: u64,
}

impl std::fmt::Debug for BulkSha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BulkSha256(<redacted>)")
    }
}

impl BulkSha256 {
    pub fn try_new() -> Result<Self, Error> {
        let context = catch_unwind(|| Context::new(&SHA256)).map_err(|_| Error::Crypto)?;
        Ok(Self {
            context: Some(context),
            bytes: 0,
        })
    }

    fn update_with(
        &mut self,
        length: usize,
        update: impl FnOnce(&mut Context) -> Result<(), Error> + UnwindSafe,
    ) -> Result<(), Error> {
        let mut context = self.context.take().ok_or(Error::Crypto)?;
        let total = u64::try_from(length)
            .ok()
            .and_then(|length| self.bytes.checked_add(length))
            .filter(|&total| total <= MAX_INPUT_BYTES)
            .ok_or(Error::Crypto)?;
        let context = catch_unwind(move || {
            update(&mut context)?;
            Ok(context)
        })
        .map_err(|_| Error::Crypto)??;
        self.context = Some(context);
        self.bytes = total;
        Ok(())
    }
}

impl Digest for BulkSha256 {
    fn update(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.update_with(bytes.len(), |context| {
            context.update(bytes);
            Ok(())
        })
    }

    fn finish(mut self) -> Result<[u8; 32], Error> {
        let context = self.context.take().ok_or(Error::Crypto)?;
        catch_unwind(move || {
            context
                .finish()
                .as_ref()
                .try_into()
                .map_err(|_| Error::Crypto)
        })
        .map_err(|_| Error::Crypto)?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bulk_matches_owned_digest_across_padding_and_chunk_boundaries() {
        let bytes: Vec<u8> = (0..1_048_577).map(|i| (i % 251) as u8).collect();
        for length in [0, 1, 55, 56, 63, 64, 65, 127, 128, 257, 65_536, bytes.len()] {
            let input = bytes.get(..length).unwrap();
            let mut owned = crate::Sha256::try_new().unwrap();
            owned.update(input).unwrap();
            let expected = owned.finish().unwrap();
            for chunk in [1, 63, 64, 65, 4096, 65_536] {
                let mut bulk = BulkSha256::try_new().unwrap();
                bulk.update(&[]).unwrap();
                for part in input.chunks(chunk) {
                    bulk.update(part).unwrap();
                }
                assert_eq!(bulk.finish().unwrap(), expected);
            }
        }
    }

    #[test]
    fn length_refusal_retires_native_state_before_update() {
        let mut bulk = BulkSha256::try_new().unwrap();
        bulk.bytes = MAX_INPUT_BYTES;
        assert_eq!(
            bulk.update_with(1, |_| panic!("must not update")),
            Err(Error::Crypto)
        );
        assert!(bulk.context.is_none());
        assert_eq!(bulk.update(&[]), Err(Error::Crypto));
        assert_eq!(bulk.finish(), Err(Error::Crypto));
    }

    #[test]
    fn returned_error_and_unwind_retire_partially_updated_state() {
        for unwind in [false, true] {
            let mut bulk = BulkSha256::try_new().unwrap();
            assert_eq!(
                bulk.update_with(3, |context| {
                    context.update(b"abc");
                    if unwind {
                        panic!("synthetic backend unwind");
                    }
                    Err(Error::Crypto)
                }),
                Err(Error::Crypto)
            );
            assert!(bulk.context.is_none());
            assert_eq!(bulk.update(b"def"), Err(Error::Crypto));
            assert_eq!(bulk.finish(), Err(Error::Crypto));
        }
    }
}
