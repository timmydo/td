#![allow(clippy::unwrap_used)]
use td_mta::{
    format::row::BlobKind,
    ids::{AccountId, BlobId},
    store_paths::{parse_blob_name, AccountEntry, Error, Name, Number, RootEntry, CAPACITY},
};

fn check(name: &Name, expected: &str) {
    assert_eq!(name.as_str().unwrap(), expected);
    assert_eq!(name.as_bytes().unwrap(), expected.as_bytes());
    assert_eq!(name.as_path().unwrap(), std::path::Path::new(expected));
    assert_eq!(name.as_c_str().unwrap().to_bytes(), expected.as_bytes());
    assert_eq!(
        name.as_c_str().unwrap().to_bytes_with_nul().last(),
        Some(&0)
    );
    assert!(expected.len() <= CAPACITY);
    assert!(!name.as_path().unwrap().is_absolute());
    assert!(name
        .as_path()
        .unwrap()
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_))));
}

#[test]
fn canonical_storage_numbers_cover_width_transitions_and_exhaustion() {
    for (value, text) in [
        (1, "00000000000000000001"),
        (42, "00000000000000000042"),
        (999_999, "00000000000000999999"),
        (1_000_000, "00000000000001000000"),
        (u64::MAX, "18446744073709551615"),
    ] {
        let number = Number::new(value).unwrap();
        assert_eq!(number.value(), value);
        assert_eq!(number.to_string(), text);
        assert_eq!(Number::parse(text), Ok(number));
    }
    assert!(
        Number::new(999_999).unwrap().to_string() < Number::new(1_000_000).unwrap().to_string()
    );
    assert_eq!(Number::new(0), Err(Error::InvalidNumber));
    for text in [
        "",
        "1",
        "00000000000000000000",
        "000000000000000000001",
        "+0000000000000000001",
        "-0000000000000000001",
        "0000000000000000000 ",
        "0000000000000000000x",
        "0000000000000000000\0",
        "000000000000000000é",
        "00001",
        "000000",
        "0000001",
        "0999999",
        "01000000",
        "18446744073709551616",
        "99999999999999999999",
        "100000000000000000000",
        "+00001",
        "-00001",
        " 00001",
        "00001 ",
        "00001\n",
        "0000\0\x31",
        "000001.log",
        "../001",
        "/00001",
        "０００００１",
        "00000é",
    ] {
        assert_eq!(Number::parse(text), Err(Error::InvalidNumber), "{text:?}");
    }
}

#[test]
fn generated_layout_matches_literal_paths() {
    for (entry, text) in [
        (RootEntry::Database, "metadata.sqlite3"),
        (RootEntry::Wal, "metadata.sqlite3-wal"),
        (RootEntry::SharedMemory, "metadata.sqlite3-shm"),
        (RootEntry::Lock, "LOCK"),
        (RootEntry::Accounts, "accounts"),
    ] {
        check(&Name::root(entry).unwrap(), text);
    }
    let account = AccountId::parse("000102030405060708090a0b0c0d0e0f").unwrap();
    for (entry, suffix) in [
        (AccountEntry::Root, ""),
        (AccountEntry::Messages, "/messages"),
        (AccountEntry::Uploads, "/uploads"),
        (AccountEntry::Temporary, "/tmp"),
        (
            AccountEntry::TemporaryFile(Number::new(42).unwrap()),
            "/tmp/00000000000000000042.tmp",
        ),
        (AccountEntry::Shard(BlobKind::Message, 255), "/messages/ff"),
    ] {
        check(
            &Name::account(account, entry).unwrap(),
            &format!("accounts/{account}{suffix}"),
        );
    }
}

#[test]
fn blob_names_cover_every_shard_and_keep_namespace_identity() {
    let account = AccountId::from_bytes([0xff; 16]);
    for shard in 0..=u8::MAX {
        let blob = BlobId::from_bytes([shard; 16]);
        for (kind, dir, extension) in [
            (BlobKind::Message, "messages", ".eml"),
            (BlobKind::Upload, "uploads", ".blob"),
        ] {
            let basename = format!("{blob}{extension}");
            assert_eq!(parse_blob_name(kind, shard, &basename), Ok(blob));
            assert_eq!(
                parse_blob_name(kind, shard.wrapping_add(1), &basename),
                Err(Error::InvalidBlobName)
            );
            check(
                &Name::account(account, AccountEntry::Shard(kind, shard)).unwrap(),
                &format!("accounts/ffffffffffffffffffffffffffffffff/{dir}/{shard:02x}"),
            );
            check(
                &Name::account(account, AccountEntry::Blob(kind, blob)).unwrap(),
                &format!("accounts/ffffffffffffffffffffffffffffffff/{dir}/{shard:02x}/{basename}"),
            );
        }
    }
}

#[test]
fn directory_entries_cannot_supply_traversal_aliases_or_extensions() {
    let id = "ab0102030405060708090a0b0c0d0e0f";
    for kind in [BlobKind::Message, BlobKind::Upload] {
        for text in [
            "".to_owned(),
            id.to_owned(),
            format!("{id}.EML"),
            format!("{id}.eml/child"),
            format!("../{id}.eml"),
            format!("/{id}.eml"),
            format!("{id}.eml\0"),
            format!("{id}.eml\n"),
            format!("{id}.eml.blob"),
            format!("{}.eml", id.to_uppercase()),
            format!("{}.blob", id.to_uppercase()),
            format!("{id}0.eml"),
            "é".repeat(16) + ".eml",
        ] {
            assert_eq!(
                parse_blob_name(kind, 0xab, &text),
                Err(Error::InvalidBlobName),
                "{text:?}"
            );
        }
    }
    assert_eq!(
        parse_blob_name(BlobKind::Upload, 0xab, &format!("{id}.eml")),
        Err(Error::InvalidBlobName)
    );
    assert_eq!(
        parse_blob_name(BlobKind::Message, 0xab, &format!("{id}.blob")),
        Err(Error::InvalidBlobName)
    );
}
