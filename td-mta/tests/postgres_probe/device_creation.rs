use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, DeviceId, StoreEpoch},
    postgres_accounts, postgres_devices as devices, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_device_creation_command_discloses_only_known_success() {
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
    let invoke = |selected_epoch: StoreEpoch, selected: AccountId, label: &str, status| {
        command_output(
            &[
                "postgres",
                "device-create",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &selected_epoch.to_string(),
                "--account",
                &selected.to_string(),
                "--label",
                label,
            ],
            fixture.root.path(),
            status,
        )
    };
    for output in [
        invoke(StoreEpoch::from_bytes([0; 16]), account, "wrong epoch", 1),
        invoke(epoch, AccountId::from_bytes([2; 16]), "missing", 1),
        invoke(epoch, account, "invalid\nlabel", 1),
    ] {
        assert!(!output.contains("password"));
        assert!(!output.contains("invalid\nlabel"));
    }
    assert!(
        devices::list(&fixture.request(), &trust, deadline(), epoch, account)
            .unwrap()
            .is_empty()
    );
    let label = "laptop α \"quoted\" \\ path";
    let output = invoke(epoch, account, label, 0);
    let parsed = td_json::parse(&output).unwrap();
    let id = DeviceId::parse(
        parsed
            .get("device")
            .and_then(td_json::Json::as_str)
            .unwrap(),
    )
    .unwrap();
    let password = parsed
        .get("password")
        .and_then(td_json::Json::as_str)
        .unwrap();
    assert_eq!(password.len(), 97);
    assert!(password.starts_with(&format!("{id}.")));
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            password.as_bytes()
        )
        .unwrap(),
        Some(id)
    );
    let listed = devices::list(&fixture.request(), &trust, deadline(), epoch, account).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].label, label);
    assert!(listed[0].created_at > 0);
    assert_eq!(listed[0].revoked_at, None);
    let revoked = command_output(
        &[
            "postgres",
            "device-revoke",
            "--connection-file",
            connection.to_str().unwrap(),
            "--epoch",
            &epoch.to_string(),
            "--account",
            &account.to_string(),
            "--device",
            &id.to_string(),
        ],
        fixture.root.path(),
        0,
    );
    assert!(!revoked.contains(password));
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            password.as_bytes()
        )
        .unwrap(),
        None
    );
    sql(&fixture,"CREATE SEQUENCE public.create_attempts; CREATE FUNCTION public.delay_device_create() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('public.create_attempts'); PERFORM pg_sleep(11); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_device_create AFTER INSERT ON td_mta.devices DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_device_create()");
    let uncertain = invoke(epoch, account, "uncertain", 1);
    assert!(uncertain.contains("indeterminate-inspect-and-revoke-device"));
    assert!(!uncertain.contains("password"));
    let parsed = td_json::parse(&uncertain).unwrap();
    let uncertain_id = DeviceId::parse(
        parsed
            .get("device")
            .and_then(td_json::Json::as_str)
            .unwrap(),
    )
    .unwrap();
    assert_ne!(uncertain_id, id);
    sql(
        &fixture,
        "DROP TRIGGER delay_device_create ON td_mta.devices",
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT last_value,is_called FROM public.create_attempts"
        ),
        "1|t"
    );
    let listed = devices::list(&fixture.request(), &trust, deadline(), epoch, account).unwrap();
    assert!(listed.len() <= 2);
    if listed.iter().any(|row| row.id == uncertain_id) {
        let revoked = command_output(
            &[
                "postgres",
                "device-revoke",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
                "--device",
                &uncertain_id.to_string(),
            ],
            fixture.root.path(),
            0,
        );
        assert!(revoked.contains("recorded"));
    }

    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    sql(&fixture,"INSERT INTO td_mta.devices(account,id,salt,verifier,label,created_at,creation_digest) SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',32),'hex'),'fixture',0,decode(repeat('00',32),'hex') FROM generate_series(1,256-(SELECT count(*)::integer FROM td_mta.devices)) n");
    let full = invoke(epoch, account, "full", 1);
    assert!(full.contains("device-capacity"));
    assert!(!full.contains("password"));
    assert_eq!(
        devices::list(&fixture.request(), &trust, deadline(), epoch, account)
            .unwrap()
            .len(),
        256
    );
}
