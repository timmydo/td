//! Incremental integrity check of one supplied authoritative blob descriptor.
use super::{CompleteFile, LockedRoot, StoreReader};
use crate::{
    format::row::{BlobKind, BlobRow},
    ids::{AccountId, BlobId},
    ports::{Crypto, CryptoError, Digest},
    store_paths::AccountEntry,
};
use std::io;

#[derive(Debug)]
pub enum BlobInputError {
    Io(io::Error),
    Crypto(CryptoError),
    Checksum,
}
impl From<io::Error> for BlobInputError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<CryptoError> for BlobInputError {
    fn from(e: CryptoError) -> Self {
        Self::Crypto(e)
    }
}
impl std::fmt::Display for BlobInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "blob input I/O: {e}"),
            Self::Crypto(e) => write!(f, "blob input digest: {e}"),
            Self::Checksum => f.write_str("blob digest mismatch"),
        }
    }
}
impl std::error::Error for BlobInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Crypto(e) => Some(e),
            Self::Checksum => None,
        }
    }
}
pub struct BlobInput<'r, 'c, C: Crypto> {
    file: StoreReader<'r>,
    crypto: &'c C,
    digest: C::Sha256,
    account: AccountId,
    id: BlobId,
    row: BlobRow,
    failed: bool,
}
impl LockedRoot {
    /// Caller supplies the final authoritative row, authorization and a live pin
    /// or actual stopped-store exclusion. Bytes remain provisional until finish.
    pub fn open_blob_input<'r, 'c, C: Crypto>(
        &'r self,
        crypto: &'c C,
        account: AccountId,
        id: BlobId,
        row: BlobRow,
        max_bytes: u64,
    ) -> Result<BlobInput<'r, 'c, C>, BlobInputError> {
        if row.length > max_bytes || max_bytes > i64::MAX as u64 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput).into());
        }
        let digest = crypto.sha256()?;
        let file = self.open_account_file(account, AccountEntry::Blob(row.kind, id), row.length)?;
        if file.len() != row.length {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        Ok(BlobInput {
            file,
            crypto,
            digest,
            account,
            id,
            row,
            failed: false,
        })
    }
}
impl<'r, C: Crypto> BlobInput<'r, '_, C> {
    pub fn len(&self) -> u64 {
        self.file.len()
    }
    pub fn is_empty(&self) -> bool {
        self.file.is_empty()
    }
    pub fn position(&self) -> u64 {
        self.file.position()
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    /// One read of at most 64 KiB; no whole-blob buffering. An error may have
    /// changed the caller slice. Empty reads never establish EOF or completion.
    pub fn read(&mut self, output: &mut [u8]) -> Result<usize, BlobInputError> {
        self.read_using(output, StoreReader::read)
    }
    fn read_using(
        &mut self,
        output: &mut [u8],
        read: impl FnOnce(&mut StoreReader<'r>, &mut [u8]) -> io::Result<usize>,
    ) -> Result<usize, BlobInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        self.failed = true;
        let count = read(&mut self.file, output)?;
        if count != 0 {
            let bytes = output
                .get(..count)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            self.digest.update(bytes)?;
        }
        self.failed = false;
        Ok(count)
    }
    pub fn finish(self) -> Result<CompleteBlob<'r>, BlobInputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        let file = self.file.finish()?;
        let digest = self.digest.finish()?;
        if !self.crypto.equal_digest(&digest, &self.row.digest) {
            return Err(BlobInputError::Checksum);
        }
        Ok(CompleteBlob {
            file,
            account: self.account,
            id: self.id,
            kind: self.row.kind,
            digest,
        })
    }
}
/// Exact private file bytes matched the supplied row; no reference/pin authority.
#[derive(Debug)]
pub struct CompleteBlob<'r> {
    file: CompleteFile<'r>,
    account: AccountId,
    id: BlobId,
    kind: BlobKind,
    digest: [u8; 32],
}
impl CompleteBlob<'_> {
    pub fn file(&self) -> &CompleteFile<'_> {
        &self.file
    }
    pub const fn account(&self) -> AccountId {
        self.account
    }
    pub const fn id(&self) -> BlobId {
        self.id
    }
    pub const fn kind(&self) -> BlobKind {
        self.kind
    }
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{tests::Fixture, MAX_FILE_STEP_BYTES};
    use super::*;
    use crate::store_paths::{Name, Number};
    use std::{fs, os::unix::fs::PermissionsExt};
    use td_crypto::{Provider, Sha256};
    pub(super) const ACCOUNT: AccountId = AccountId::from_bytes([0x42; 16]);
    pub(super) const BLOB: BlobId = BlobId::from_bytes([0xee; 16]);
    pub(super) fn row(kind: BlobKind, bytes: &[u8]) -> BlobRow {
        let mut digest = Provider.sha256().unwrap();
        digest.update(bytes).unwrap();
        BlobRow {
            kind,
            length: bytes.len() as u64,
            digest: digest.finish().unwrap(),
            created_at: 0,
        }
    }
    pub(super) fn install(root: &LockedRoot, kind: BlobKind, number: u64, bytes: &[u8]) {
        let mut file = root
            .create_temporary(ACCOUNT, Number::new(number).unwrap(), bytes.len() as u64)
            .unwrap();
        for chunk in bytes.chunks(MAX_FILE_STEP_BYTES) {
            file.write(chunk).unwrap();
        }
        file.sync().unwrap().publish_blob(kind, BLOB).unwrap();
    }
    fn setup(fixture: &Fixture) -> LockedRoot {
        let root = fixture.locked();
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Temporary,
            AccountEntry::Messages,
            AccountEntry::Uploads,
            AccountEntry::Shard(BlobKind::Message, 0xee),
            AccountEntry::Shard(BlobKind::Upload, 0xee),
        ] {
            root.create_account_directory(ACCOUNT, entry).unwrap();
        }
        root
    }
    fn path(fixture: &Fixture, kind: BlobKind) -> std::path::PathBuf {
        fixture.path.join(
            Name::account(ACCOUNT, AccountEntry::Blob(kind, BLOB))
                .unwrap()
                .as_path()
                .unwrap(),
        )
    }
    #[test]
    fn exact_blob_bytes_match_known_digest_and_preserve_identity() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        for (i, kind) in [BlobKind::Message, BlobKind::Upload]
            .into_iter()
            .enumerate()
        {
            install(&root, kind, i as u64 + 1, b"abc");
            let mut expected = row(kind, b"abc");
            expected.digest = [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ];
            let mut input = root
                .open_blob_input(&Provider, ACCOUNT, BLOB, expected, 3)
                .unwrap();
            assert_eq!(input.len(), 3);
            assert!(!input.is_empty());
            assert_eq!(input.read(&mut []).unwrap(), 0);
            assert_eq!(input.position(), 0);
            let mut bytes = [0; 2];
            assert_eq!(input.read(&mut bytes).unwrap(), 2);
            assert_eq!(&bytes, b"ab");
            assert_eq!(input.read(&mut bytes).unwrap(), 1);
            assert_eq!(bytes[0], b'c');
            assert_eq!(input.read(&mut bytes).unwrap(), 0);
            assert_eq!(input.position(), 3);
            let complete = input.finish().unwrap();
            assert_eq!(complete.account(), ACCOUNT);
            assert_eq!(complete.id(), BLOB);
            assert_eq!(complete.kind(), kind);
            assert_eq!(complete.digest(), &expected.digest);
            assert_eq!(
                complete.file().name(),
                &Name::account(ACCOUNT, AccountEntry::Blob(kind, BLOB)).unwrap()
            );
            assert_eq!(complete.file().read_at(1, &mut bytes).unwrap(), 2);
            assert_eq!(&bytes, b"bc");
        }
    }
    #[test]
    fn empty_and_large_blobs_use_exact_extents_and_bounded_reads() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        install(&root, BlobKind::Upload, 1, b"");
        let input = root
            .open_blob_input(&Provider, ACCOUNT, BLOB, row(BlobKind::Upload, b""), 0)
            .unwrap();
        assert!(input.is_empty());
        assert_eq!(input.finish().unwrap().file().len(), 0);
        let data = vec![0x5a; MAX_FILE_STEP_BYTES + 1];
        install(&root, BlobKind::Message, 2, &data);
        let mut input = root
            .open_blob_input(
                &Provider,
                ACCOUNT,
                BLOB,
                row(BlobKind::Message, &data),
                data.len() as u64,
            )
            .unwrap();
        let mut buffer = vec![0; data.len()];
        assert_eq!(input.read(&mut buffer).unwrap(), MAX_FILE_STEP_BYTES);
        assert_eq!(buffer.last(), Some(&0));
        assert_eq!(input.read(&mut buffer).unwrap(), 1);
        assert_eq!(input.finish().unwrap().file().len(), data.len() as u64);
    }
    #[test]
    fn missing_nonprivate_and_wrong_size_inputs_refuse() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        let expected = row(BlobKind::Message, b"abc");
        assert!(
            matches!(root.open_blob_input(&Provider,ACCOUNT,BLOB,expected,3),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::NotFound)
        );
        install(&root, BlobKind::Message, 1, b"abc");
        for max in [2, u64::MAX] {
            assert!(
                matches!(root.open_blob_input(&Provider,ACCOUNT,BLOB,expected,max),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
            );
        }
        for length in [2, 4] {
            assert!(
                matches!(root.open_blob_input(&Provider,ACCOUNT,BLOB,BlobRow{length,..expected},4),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
            );
        }
        fs::set_permissions(
            path(&fixture, BlobKind::Message),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(
            matches!(root.open_blob_input(&Provider,ACCOUNT,BLOB,expected,3),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
        );
    }
    #[test]
    fn corruption_incomplete_consumption_and_changed_extents_never_complete() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        install(&root, BlobKind::Message, 1, b"abc");
        let expected = row(BlobKind::Message, b"abc");
        let mut bytes = [0; 3];
        let input = root
            .open_blob_input(&Provider, ACCOUNT, BLOB, expected, 3)
            .unwrap();
        assert!(
            matches!(input.finish(),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidInput)
        );
        for length in [2, 4] {
            fs::write(path(&fixture, BlobKind::Message), b"abc").unwrap();
            let mut input = root
                .open_blob_input(&Provider, ACCOUNT, BLOB, expected, 3)
                .unwrap();
            assert_eq!(input.read(&mut bytes).unwrap(), 3);
            fs::write(path(&fixture, BlobKind::Message), vec![0; length]).unwrap();
            assert!(
                matches!(input.finish(),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::InvalidData)
            );
        }
        fs::write(path(&fixture, BlobKind::Message), b"abd").unwrap();
        let mut input = root
            .open_blob_input(&Provider, ACCOUNT, BLOB, expected, 3)
            .unwrap();
        input.read(&mut bytes).unwrap();
        assert!(matches!(input.finish(), Err(BlobInputError::Checksum)));
    }
    #[test]
    fn read_and_crypto_failures_retire_the_input() {
        let fixture = Fixture::new();
        let root = setup(&fixture);
        install(&root, BlobKind::Message, 1, b"abc");
        let expected = row(BlobKind::Message, b"abc");
        for kind in [io::ErrorKind::Interrupted, io::ErrorKind::PermissionDenied] {
            let mut input = root
                .open_blob_input(&Provider, ACCOUNT, BLOB, expected, 3)
                .unwrap();
            assert!(
                matches!(input.read_using(&mut[0;3],|_,_|Err(kind.into())),Err(BlobInputError::Io(e)) if e.kind()==kind)
            );
            assert!(input.is_failed());
            assert!(
                matches!(input.read(&mut[0;3]),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
            assert!(
                matches!(input.finish(),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
            );
        }
        assert!(matches!(
            root.open_blob_input(&Fault(0), ACCOUNT, BLOB, expected, 3),
            Err(BlobInputError::Crypto(CryptoError::Crypto))
        ));
        let fault = Fault(1);
        let mut input = root
            .open_blob_input(&fault, ACCOUNT, BLOB, expected, 3)
            .unwrap();
        assert_eq!(input.read(&mut []).unwrap(), 0);
        assert!(matches!(
            input.read(&mut [0; 3]),
            Err(BlobInputError::Crypto(CryptoError::Crypto))
        ));
        assert!(input.is_failed());
        assert_eq!(input.position(), 3);
        assert!(
            matches!(input.finish(),Err(BlobInputError::Io(e)) if e.kind()==io::ErrorKind::BrokenPipe)
        );
        let fault = Fault(2);
        let mut input = root
            .open_blob_input(&fault, ACCOUNT, BLOB, expected, 3)
            .unwrap();
        input.read(&mut [0; 3]).unwrap();
        assert!(matches!(
            input.finish(),
            Err(BlobInputError::Crypto(CryptoError::Crypto))
        ));
    }
    struct Fault(u8);
    struct FaultDigest {
        point: u8,
        inner: Sha256,
        failed: bool,
    }
    impl Digest for FaultDigest {
        fn update(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
            if self.failed || self.point == 1 {
                self.failed = true;
                return Err(CryptoError::Crypto);
            }
            let result = self.inner.update(bytes);
            self.failed |= result.is_err();
            result
        }
        fn finish(self) -> Result<[u8; 32], CryptoError> {
            if self.failed || self.point == 2 {
                return Err(CryptoError::Crypto);
            }
            self.inner.finish()
        }
    }
    impl Crypto for Fault {
        type Sha256 = FaultDigest;
        type SigningKey = ();
        fn sha256(&self) -> Result<FaultDigest, CryptoError> {
            if self.0 == 0 {
                return Err(CryptoError::Crypto);
            }
            Ok(FaultDigest {
                point: self.0,
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
}
