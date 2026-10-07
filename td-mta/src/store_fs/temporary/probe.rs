//! Actual raw-file operations under the dedicated Rust allocation counter.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::{format::row::BlobKind, ids::BlobId};

pub fn run(mut snapshot: impl FnMut()) {
    for fixture in [
        super::super::tests::Fixture::new(),
        super::super::tests::Fixture::maximum_root(),
    ] {
        let root = fixture.locked();
        let account = AccountId::from_bytes([0x42; 16]);
        let id = BlobId::from_bytes([0x44; 16]);
        root.create_accounts_directory().unwrap();
        for entry in [
            AccountEntry::Root,
            AccountEntry::Temporary,
            AccountEntry::Messages,
            AccountEntry::Shard(BlobKind::Message, 0x44),
        ] {
            root.create_account_directory(account, entry).unwrap();
        }
        snapshot();
        let mut temporary = root
            .create_temporary(account, Number::new(1).unwrap(), 3)
            .unwrap();
        temporary.write(b"abc").unwrap();
        let published = temporary
            .sync()
            .unwrap()
            .publish_blob(BlobKind::Message, id)
            .unwrap();
        assert_eq!(published.len(), 3);
        drop(published);
        snapshot();
        snapshot();
        let mut temporary = root
            .create_temporary(account, Number::new(2).unwrap(), 3)
            .unwrap();
        temporary.write(b"xyz").unwrap();
        assert!(temporary
            .sync()
            .unwrap()
            .publish_blob(BlobKind::Message, id)
            .is_err());
        snapshot();
        snapshot();
        let mut input = root
            .open_account_file(account, AccountEntry::Blob(BlobKind::Message, id), 3)
            .unwrap();
        let mut bytes = [0; 4];
        assert_eq!(input.read(&mut bytes).unwrap(), 3);
        assert_eq!(bytes.get(..3), Some(b"abc".as_slice()));
        assert_eq!(input.finish().unwrap().len(), 3);
        snapshot();
        snapshot();
        assert!(root
            .open_account_file(account, AccountEntry::Blob(BlobKind::Message, id), 2)
            .is_err());
        assert!(root
            .create_temporary(account, Number::new(2).unwrap(), 3)
            .is_err());
        snapshot();
    }
}
