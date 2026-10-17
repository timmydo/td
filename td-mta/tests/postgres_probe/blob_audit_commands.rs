use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_accounts, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_blob_audit_command_checks_bytes_without_repair() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let blob = BlobId::from_bytes([3; 16]);
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("connection");
    fs::write(
        &connection,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let invoke = |selected_epoch: StoreEpoch, selected_account: AccountId, status| {
        let output = command_output(
            &[
                "postgres",
                "blob-audit",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &selected_epoch.to_string(),
                "--account",
                &selected_account.to_string(),
                "--blob",
                &blob.to_string(),
            ],
            fixture.root.path(),
            status,
        );
        assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
        output
    };
    assert!(invoke(epoch, account, 1).contains("blob-not-found"));
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    assert!(invoke(epoch, account, 1).contains("blob-not-found"));
    sql(&fixture,"INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),3,decode('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad','hex'),0,decode('0000000000000001','hex'),1); INSERT INTO td_mta.blob_chunks VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode('616263','hex'))");
    let output = invoke(epoch, account, 0);
    let parsed = td_json::parse(&output).unwrap();
    for (key, value) in [
        ("command", "postgres-blob-audit".to_owned()),
        ("account", account.to_string()),
        ("blob", blob.to_string()),
        ("store_epoch", epoch.to_string()),
    ] {
        assert_eq!(
            parsed.get(key).and_then(td_json::Json::as_str),
            Some(value.as_str())
        );
    }
    for (key, value) in [
        ("schema", 1),
        ("sequence", 1),
        ("history_floor", 0),
        ("verified_bytes", 3),
        ("created_at", 0),
    ] {
        assert_eq!(parsed.get(key).and_then(td_json::Json::as_u64), Some(value));
    }
    assert_eq!(
        parsed
            .get("body_digest_verified")
            .and_then(td_json::Json::as_bool),
        Some(true)
    );
    assert_eq!(
        parsed
            .get("semantic_verified")
            .and_then(td_json::Json::as_bool),
        Some(false)
    );
    assert!(invoke(StoreEpoch::from_bytes([0; 16]), account, 1).contains("epoch-mismatch"));
    assert!(invoke(epoch, AccountId::from_bytes([2; 16]), 1).contains("blob-not-found"));
    sql(
        &fixture,
        "UPDATE td_mta.blob_chunks SET bytes=decode('78797a','hex')",
    );
    assert!(invoke(epoch, account, 1).contains("body-check-failed"));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(bytes,'hex') FROM td_mta.blob_chunks"
        ),
        "78797a"
    );
    sql(&fixture,"DELETE FROM td_mta.blob_chunks; UPDATE td_mta.blobs SET length=0,chunk_count=0,digest=decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex')");
    let empty = invoke(epoch, account, 0);
    assert_eq!(
        td_json::parse(&empty)
            .unwrap()
            .get("verified_bytes")
            .and_then(td_json::Json::as_u64),
        Some(0)
    );
    sql(
        &fixture,
        "UPDATE td_mta.blobs SET digest=decode(repeat('00',32),'hex')",
    );
    assert!(invoke(epoch, account, 1).contains("body-check-failed"));
    assert_eq!(
        sql(&fixture, "SELECT encode(digest,'hex') FROM td_mta.blobs"),
        "00".repeat(32)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT body_bytes,blob_count FROM td_mta.account_usage"
        ),
        "0|0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001"
    );
    for extra in ["--account", "--epoch", "--blob", "--repair", "--profile"] {
        let output = command_output(
            &[
                "postgres",
                "blob-audit",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
                "--blob",
                &blob.to_string(),
                extra,
                &blob.to_string(),
            ],
            fixture.root.path(),
            2,
        );
        assert!(output.contains("invalid-arguments"));
    }
}
