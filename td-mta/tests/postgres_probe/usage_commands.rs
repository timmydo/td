use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_usage_command_reports_counters_without_repair() {
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
    let invoke = |selected_epoch: StoreEpoch, status| {
        let output = command_output(
            &[
                "postgres",
                "usage-inspect",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &selected_epoch.to_string(),
                "--account",
                &account.to_string(),
            ],
            fixture.root.path(),
            status,
        );
        assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
        output
    };
    assert!(invoke(epoch, 1).contains("account-not-found"));
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let output = invoke(epoch, 0);
    let parsed = td_json::parse(&output).unwrap();
    assert_eq!(
        parsed.get("account").and_then(td_json::Json::as_str),
        Some(account.to_string().as_str())
    );
    assert_eq!(
        parsed.get("store_epoch").and_then(td_json::Json::as_str),
        Some(epoch.to_string().as_str())
    );
    assert_eq!(
        parsed
            .get("counters_consistent")
            .and_then(td_json::Json::as_bool),
        Some(true)
    );
    assert_eq!(
        parsed
            .get("semantic_verified")
            .and_then(td_json::Json::as_bool),
        Some(false)
    );
    for (key, value) in [
        ("sequence", 1),
        ("history_floor", 0),
        ("body_bytes", 0),
        ("blob_count", 0),
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
    assert!(invoke(StoreEpoch::from_bytes([0; 16]), 1).contains("epoch-mismatch"));
    sql(&fixture,"INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0); UPDATE td_mta.account_usage SET blob_count=1");
    let output = invoke(epoch, 0);
    assert_eq!(
        td_json::parse(&output)
            .unwrap()
            .get("blob_count")
            .and_then(td_json::Json::as_u64),
        Some(1)
    );
    sql(&fixture, "UPDATE td_mta.account_usage SET blob_count=2");
    assert!(invoke(epoch, 1).contains("usage-integrity"));
    assert_eq!(
        sql(&fixture, "SELECT blob_count FROM td_mta.account_usage"),
        "2"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001"
    );
    for extra in ["--account", "--epoch", "--repair", "--profile"] {
        let output = command_output(
            &[
                "postgres",
                "usage-inspect",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
                extra,
                &account.to_string(),
            ],
            fixture.root.path(),
            2,
        );
        assert!(output.contains("invalid-arguments"));
    }
}
