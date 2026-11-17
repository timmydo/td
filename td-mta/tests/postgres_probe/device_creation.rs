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
    // Fill to the 256 unrevoked devices; earlier revocations do not count.
    let count = |fixture: &Fixture, filter: &str| -> usize {
        sql(
            fixture,
            &format!("SELECT count(*) FROM td_mta.devices{filter}"),
        )
        .parse()
        .unwrap()
    };
    let listed = |fixture: &Fixture| {
        devices::list(&fixture.request(), &trust, deadline(), epoch, account)
            .unwrap()
            .len()
    };
    sql(&fixture,"INSERT INTO td_mta.devices(account,id,salt,verifier,label,created_at,creation_digest) SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',32),'hex'),'fixture',0,decode(repeat('00',32),'hex') FROM generate_series(1,256-(SELECT count(*)::integer FROM td_mta.devices WHERE revoked_at IS NULL)) n");
    assert_eq!(count(&fixture, " WHERE revoked_at IS NULL"), 256);
    let full = invoke(epoch, account, "full", 1);
    assert!(full.contains("device-capacity"));
    assert!(!full.contains("password"));
    assert_eq!(listed(&fixture), count(&fixture, ""));

    // Revoked devices free their unrevoked slots.
    const NOW: &str = "(extract(epoch FROM clock_timestamp())*1000)::bigint";
    sql(&fixture, &format!("UPDATE td_mta.devices SET revoked_at={NOW},revocation_id=id,revocation_digest=decode(repeat('00',32),'hex') WHERE label='fixture'"));
    assert!(invoke(epoch, account, "after revocation", 0).contains("password"));
    // Revoked records awaiting retirement still count against 320.
    sql(&fixture, &format!("INSERT INTO td_mta.devices(account,id,salt,verifier,label,created_at,creation_digest,revoked_at,revocation_id,revocation_digest) SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',32),'hex'),'fixture',0,decode(repeat('00',32),'hex'),{NOW},decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex') FROM generate_series(4096,4096+319-(SELECT count(*)::integer FROM td_mta.devices)) n"));
    assert_eq!(count(&fixture, ""), 320);
    assert_eq!(listed(&fixture), 320);
    assert!(invoke(epoch, account, "records", 1).contains("device-capacity"));
    // Records revoked over 30 days ago by the database clock retire at the
    // next creation; a recent revocation stays however old its creation.
    sql(&fixture, &format!("UPDATE td_mta.devices SET revoked_at={NOW}-{}-1 WHERE label='fixture' AND id<>decode(lpad(to_hex(4096),32,'0'),'hex')", devices::REVOKED_RETENTION_MS));
    assert!(invoke(epoch, account, "after retirement", 0).contains("password"));
    assert_eq!(count(&fixture, " WHERE label='fixture'"), 1);
    assert_eq!(listed(&fixture), count(&fixture, ""));

    // A revocation records the database's time, whatever was requested; a
    // retired credential stays unusable and cannot be revoked again.
    let retired = devices::prepare_creation(epoch, account, "retired", 1).unwrap();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &retired).unwrap();
    let check = |password: &[u8]| {
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            password,
        )
    };
    assert_eq!(check(retired.password()), Ok(Some(retired.device_id())));
    let revoke = devices::prepare_revocation(epoch, account, retired.device_id(), 2).unwrap();
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoke),
        Ok(devices::RevocationOutcome::Recorded)
    );
    let id = format!("decode('{}','hex')", retired.device_id());
    assert_eq!(
        sql(
            &fixture,
            &format!("SELECT abs(revoked_at-{NOW})<600000 FROM td_mta.devices WHERE id={id}")
        ),
        "t"
    );
    assert_eq!(check(retired.password()), Ok(None));
    sql(
        &fixture,
        &format!(
            "UPDATE td_mta.devices SET revoked_at=revoked_at-{}-1 WHERE id={id}",
            devices::REVOKED_RETENTION_MS
        ),
    );
    let later = devices::prepare_creation(epoch, account, "later", 1).unwrap();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &later).unwrap();
    assert_eq!(count(&fixture, &format!(" WHERE id={id}")), 0);
    assert_eq!(check(retired.password()), Ok(None));
    let again = devices::prepare_revocation(epoch, account, retired.device_id(), 3).unwrap();
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &again),
        Err(devices::Error::Missing)
    );
}
