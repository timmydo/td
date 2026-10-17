use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_metadata_audit_command_checks_graph_without_bodies() {
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
    let invoke = |selected_epoch: StoreEpoch,
                  selected_account: AccountId,
                  rows: &str,
                  parents: &str,
                  status| {
        let output = command_output(
            &[
                "postgres",
                "metadata-audit",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &selected_epoch.to_string(),
                "--account",
                &selected_account.to_string(),
                "--max-rows",
                rows,
                "--max-parent-reads",
                parents,
            ],
            fixture.root.path(),
            status,
        );
        assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
        output
    };
    assert!(invoke(epoch, account, "100", "100", 1).contains("account-not-found"));
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let output = invoke(epoch, account, "100", "100", 0);
    let parsed = td_json::parse(&output).unwrap();
    for (key, value) in [
        ("command", "postgres-metadata-audit".to_owned()),
        ("account", account.to_string()),
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
        ("metadata_rows", 1),
        ("mailboxes", 1),
    ] {
        assert_eq!(parsed.get(key).and_then(td_json::Json::as_u64), Some(value));
    }
    for (key, value) in [
        ("canonical_metadata_verified", true),
        ("body_digests_verified", false),
        ("semantic_verified", false),
    ] {
        assert_eq!(
            parsed.get(key).and_then(td_json::Json::as_bool),
            Some(value)
        );
    }
    assert!(
        invoke(StoreEpoch::from_bytes([0; 16]), account, "100", "100", 1)
            .contains("epoch-mismatch")
    );
    assert!(
        invoke(epoch, AccountId::from_bytes([2; 16]), "100", "100", 1)
            .contains("account-not-found")
    );
    assert!(invoke(epoch, account, "0", "100", 1).contains("metadata-check-failed"));
    assert!(invoke(epoch, account, "100", "0", 1).contains("metadata-check-failed"));
    sql(&fixture, "UPDATE td_mta.mailboxes SET name=chr(10)");
    assert!(invoke(epoch, account, "100", "100", 1).contains("metadata-check-failed"));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(convert_to(name,'UTF8'),'hex') FROM td_mta.mailboxes"
        ),
        "0a"
    );
    sql(&fixture,"UPDATE td_mta.mailboxes SET name='Inbox'; INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode(repeat('00',32),'hex'),0,decode('0000000000000001','hex'),0)");
    // A metadata report does not verify body bytes, counters or role uniqueness.
    sql(&fixture,"INSERT INTO td_mta.mailboxes SELECT account,decode(repeat('09',16),'hex'),'Duplicate',NULL,role,0,1,changed FROM td_mta.mailboxes WHERE role='inbox'");
    let scoped = invoke(epoch, account, "100", "100", 0);
    assert_eq!(
        td_json::parse(&scoped)
            .unwrap()
            .get("metadata_rows")
            .and_then(td_json::Json::as_u64),
        Some(3)
    );
    assert_eq!(
        sql(&fixture, "SELECT encode(digest,'hex') FROM td_mta.blobs"),
        "00".repeat(32)
    );
    assert_eq!(
        sql(&fixture, "SELECT blob_count FROM td_mta.account_usage"),
        "0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001"
    );
    for extra in [
        "--account",
        "--epoch",
        "--max-rows",
        "--max-parent-reads",
        "--repair",
        "--max-blobs",
    ] {
        let output = command_output(
            &[
                "postgres",
                "metadata-audit",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
                "--max-rows",
                "100",
                "--max-parent-reads",
                "100",
                extra,
                "1",
            ],
            fixture.root.path(),
            2,
        );
        assert!(output.contains("invalid-arguments"));
    }
}
