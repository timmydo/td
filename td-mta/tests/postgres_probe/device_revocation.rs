use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, DeviceId, StoreEpoch},
    postgres_accounts, postgres_devices as devices, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_device_revocation_command_preserves_receipts() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let creation = devices::prepare_creation(epoch, account, "revoke-command", 10).unwrap();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        )
        .unwrap(),
        Some(creation.device_id())
    );
    let sequence = sql(
        &fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
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
    let invoke = |epoch: StoreEpoch, selected: AccountId, device: DeviceId, status| {
        command_output(
            &[
                "postgres",
                "device-revoke",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &selected.to_string(),
                "--device",
                &device.to_string(),
            ],
            fixture.root.path(),
            status,
        )
    };
    assert!(invoke(
        StoreEpoch::from_bytes([0; 16]),
        account,
        creation.device_id(),
        1
    )
    .contains("epoch-mismatch"));
    assert!(invoke(
        epoch,
        AccountId::from_bytes([2; 16]),
        creation.device_id(),
        1
    )
    .contains("account-or-device-not-found"));
    assert!(invoke(epoch, account, DeviceId::from_bytes([0; 16]), 1)
        .contains("account-or-device-not-found"));
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        )
        .unwrap(),
        Some(creation.device_id())
    );
    let output = invoke(epoch, account, creation.device_id(), 0);
    let parsed = td_json::parse(&output).unwrap();
    assert_eq!(
        parsed.get("outcome").and_then(td_json::Json::as_str),
        Some("recorded")
    );
    let operation = parsed
        .get("operation")
        .and_then(td_json::Json::as_str)
        .unwrap();
    let receipt = sql(&fixture, "SELECT encode(revocation_id,'hex') || '|' || encode(revocation_digest,'hex') || '|' || revoked_at FROM td_mta.devices");
    assert!(receipt.starts_with(&format!("{operation}|")));
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        )
        .unwrap(),
        None
    );
    assert!(
        devices::list(&fixture.request(), &trust, deadline(), epoch, account).unwrap()[0]
            .revoked_at
            .is_some()
    );
    let repeated = invoke(epoch, account, creation.device_id(), 0);
    assert!(repeated.contains("already-revoked"));
    assert_eq!(sql(&fixture, "SELECT encode(revocation_id,'hex') || '|' || encode(revocation_digest,'hex') || '|' || revoked_at FROM td_mta.devices"),receipt);
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    let refused = command_output(
        &[
            "postgres",
            "device-revoke",
            "--connection-file",
            "not-for-output",
            "--epoch",
            &epoch.to_string(),
            "--account",
            &account.to_string(),
            "--device",
            &creation.device_id().to_string(),
            "--device",
            &creation.device_id().to_string(),
        ],
        fixture.root.path(),
        2,
    );
    assert!(refused.contains("invalid-arguments"));
    assert!(!refused.contains("not-for-output"));
    let delayed = devices::prepare_creation(epoch, account, "uncertain-revoke", 11).unwrap();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &delayed).unwrap();
    sql(&fixture, "CREATE SEQUENCE public.revoke_attempts; CREATE FUNCTION public.delay_device_revoke() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('public.revoke_attempts'); PERFORM pg_sleep(11); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_device_revoke AFTER UPDATE ON td_mta.devices DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_device_revoke()");
    let uncertain = invoke(epoch, account, delayed.device_id(), 1);
    assert!(uncertain.contains("indeterminate-inspect-device"));
    assert!(!uncertain.contains("\"status\":\"ok\""));
    // DDL waits for the original transaction, then removes only fixture delay.
    sql(
        &fixture,
        "DROP TRIGGER delay_device_revoke ON td_mta.devices",
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT last_value,is_called FROM public.revoke_attempts"
        ),
        "1|t"
    );
    let settled = devices::list(&fixture.request(), &trust, deadline(), epoch, account).unwrap();
    let was_revoked = settled
        .iter()
        .find(|row| row.id == delayed.device_id())
        .unwrap()
        .revoked_at
        .is_some();
    let final_output = invoke(epoch, account, delayed.device_id(), 0);
    let parsed = td_json::parse(&final_output).unwrap();
    assert_eq!(
        parsed.get("outcome").and_then(td_json::Json::as_str),
        Some(if was_revoked {
            "already-revoked"
        } else {
            "recorded"
        })
    );
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            delayed.password()
        )
        .unwrap(),
        None
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
}
