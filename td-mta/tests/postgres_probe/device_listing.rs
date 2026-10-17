use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts, postgres_devices as devices,
    postgres_probe::Request,
    postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_device_listing_excludes_verifiers_and_refuses_overflow() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let other = AccountId::from_bytes([2; 16]);
    for selected in [account, other] {
        postgres_accounts::publish(
            &fixture.request(),
            &trust,
            deadline(),
            postgres_accounts::prepare(epoch, selected).unwrap(),
        )
        .unwrap();
    }
    let list = |selected| devices::list(&fixture.request(), &trust, deadline(), epoch, selected);
    assert!(list(account).unwrap().is_empty());
    assert_eq!(
        list(AccountId::from_bytes([3; 16])),
        Err(devices::Error::Missing)
    );
    let label = "laptop α \"quoted\" \\ path";
    let creation = devices::prepare_creation(epoch, account, label, 10).unwrap();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    let expected = devices::DeviceSummary {
        id: creation.device_id(),
        label: label.to_owned(),
        created_at: 10,
        revoked_at: None,
    };
    assert_eq!(list(account).unwrap(), vec![expected.clone()]);
    assert!(list(other).unwrap().is_empty());
    assert_eq!(
        devices::list(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account
        ),
        Err(devices::Error::Epoch)
    );
    assert!(devices::list(
        &fixture.request(),
        &trust,
        std::time::Instant::now(),
        epoch,
        account
    )
    .is_err());
    let sequence = sql(
        &fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts ORDER BY id",
    );
    sql(&fixture,"CREATE ROLE device_metadata LOGIN PASSWORD 'disposable-postgres-fixture-password'; GRANT USAGE ON SCHEMA td_mta TO device_metadata; GRANT SELECT ON td_mta.schema_migrations,td_mta.accounts TO device_metadata; GRANT SELECT(account,id,label,created_at,revoked_at) ON td_mta.devices TO device_metadata");
    assert_eq!(sql(&fixture,"SELECT has_column_privilege('device_metadata','td_mta.devices','salt','SELECT'),has_column_privilege('device_metadata','td_mta.devices','verifier','SELECT')"),"f|f");
    let reader = Request {
        user: "device_metadata",
        ..fixture.request()
    };
    assert_eq!(
        devices::list(&reader, &trust, deadline(), epoch, account).unwrap(),
        vec![expected.clone()]
    );

    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("connection");
    fs::write(
        &connection,
        format!(
            "postgresql://device_metadata:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let epoch_text = epoch.to_string();
    let account_text = account.to_string();
    let invoke = |epoch: &str, status| {
        command_output(
            &[
                "postgres",
                "device-list",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                epoch,
                "--account",
                &account_text,
            ],
            fixture.root.path(),
            status,
        )
    };
    let output = invoke(&epoch_text, 0);
    let parsed = td_json::parse(&output).unwrap();
    assert_eq!(
        parsed.get("status").and_then(td_json::Json::as_str),
        Some("ok")
    );
    let rows = parsed.get("devices").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("label").and_then(td_json::Json::as_str),
        Some(label)
    );
    assert_eq!(
        rows[0].get("id").and_then(td_json::Json::as_str),
        Some(creation.device_id().to_string().as_str())
    );
    assert!(rows[0].get("revoked_at").unwrap().is_null());
    assert_eq!(rows[0].as_object().unwrap().len(), 4);
    assert!(invoke(&StoreEpoch::from_bytes([0; 16]).to_string(), 1).contains("epoch-mismatch"));
    let refused = command_output(
        &[
            "postgres",
            "device-list",
            "--connection-file",
            "not-for-output",
            "--epoch",
            &epoch_text,
            "--account",
            &account_text,
            "--account",
            &account_text,
        ],
        fixture.root.path(),
        2,
    );
    assert!(refused.contains("invalid-arguments"));
    assert!(!refused.contains("not-for-output"));
    let revoked = devices::prepare_revocation(epoch, account, creation.device_id(), 20).unwrap();
    devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoked).unwrap();
    assert_eq!(
        list(account).unwrap(),
        vec![devices::DeviceSummary {
            revoked_at: Some(20),
            ..expected
        }]
    );
    assert!(invoke(&epoch_text, 0).contains("\"revoked_at\":20"));
    sql(
        &fixture,
        "UPDATE td_mta.devices SET label=E'invalid\\nlabel'",
    );
    assert_eq!(list(account), Err(devices::Error::Invalid));
    assert!(invoke(&epoch_text, 1).contains("invalid-device-metadata"));
    sql(&fixture, "UPDATE td_mta.devices SET label='restored'");
    sql(&fixture,"INSERT INTO td_mta.devices(account,id,salt,verifier,label,created_at,creation_digest) SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',32),'hex'),'fixture',0,decode(repeat('00',32),'hex') FROM generate_series(1,255) n");
    let full = list(account).unwrap();
    assert_eq!(full.len(), 256);
    assert!(full
        .windows(2)
        .all(|rows| rows[0].id.as_bytes() < rows[1].id.as_bytes()));
    sql(&fixture,"INSERT INTO td_mta.devices(account,id,salt,verifier,label,created_at,creation_digest) VALUES(decode(repeat('01',16),'hex'),decode(repeat('ff',16),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',32),'hex'),'overflow',0,decode(repeat('00',32),'hex'))");
    assert_eq!(list(account), Err(devices::Error::Capacity));
    assert!(invoke(&epoch_text, 1).contains("device-capacity"));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts ORDER BY id"
        ),
        sequence
    );
}
