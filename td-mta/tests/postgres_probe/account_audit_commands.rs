use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_account_audit_command_reports_one_snapshot_without_repair() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
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
    let invoke = |selected: StoreEpoch, max_blobs: &str, code| {
        let output = command_output(
            &[
                "postgres",
                "account-audit",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &selected.to_string(),
                "--account",
                &account.to_string(),
                "--max-rows",
                "100",
                "--max-parent-reads",
                "100",
                "--max-blobs",
                max_blobs,
                "--max-bytes",
                "0",
            ],
            fixture.root.path(),
            code,
        );
        assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
        output
    };
    assert!(invoke(epoch, "1", 1).contains("account-not-found"));
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    sql(&fixture,"INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0); UPDATE td_mta.account_usage SET blob_count=1");
    let output = invoke(epoch, "1", 0);
    let parsed = td_json::parse(&output).unwrap();
    assert_eq!(
        parsed.get("account").and_then(td_json::Json::as_str),
        Some(account.to_string().as_str())
    );
    assert_eq!(
        parsed.get("store_epoch").and_then(td_json::Json::as_str),
        Some(epoch.to_string().as_str())
    );
    for (key, value) in [
        ("sequence", 1),
        ("history_floor", 0),
        ("metadata_rows", 2),
        ("verified_blobs", 1),
        ("verified_bytes", 0),
        ("body_bytes", 0),
        ("blob_count", 1),
        ("upload_bytes", 0),
        ("queue_bytes", 0),
        ("queue_submissions", 0),
    ] {
        assert_eq!(
            parsed.get(key).and_then(td_json::Json::as_u64),
            Some(value),
            "{key}"
        );
    }
    for key in [
        "canonical_metadata_verified",
        "counters_consistent",
        "body_digests_verified",
    ] {
        assert_eq!(parsed.get(key).and_then(td_json::Json::as_bool), Some(true));
    }
    assert_eq!(
        parsed
            .get("semantic_verified")
            .and_then(td_json::Json::as_bool),
        Some(false)
    );
    assert!(invoke(epoch, "0", 1).contains("body-limit"));
    assert!(invoke(StoreEpoch::from_bytes([0; 16]), "1", 1).contains("epoch-mismatch"));
    for limit in ["+1", "-1", " 1", "18446744073709551616"] {
        assert!(invoke(epoch, limit, 2).contains("invalid-arguments"));
    }
    sql(&fixture, "UPDATE td_mta.account_usage SET blob_count=2");
    assert!(invoke(epoch, "1", 1).contains("usage-integrity"));
    assert_eq!(
        sql(&fixture, "SELECT blob_count FROM td_mta.account_usage"),
        "2"
    );
    sql(&fixture,"UPDATE td_mta.account_usage SET blob_count=1; UPDATE td_mta.blobs SET digest=decode(repeat('00',32),'hex')");
    assert!(invoke(epoch, "1", 1).contains("body-check-failed"));
    assert_eq!(
        sql(&fixture, "SELECT encode(digest,'hex') FROM td_mta.blobs"),
        "00".repeat(32)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001"
    );
}
