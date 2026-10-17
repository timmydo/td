use super::{
    command_output, create_runtime_login, deadline, grant_runtime_mail, sql, Fixture, PASSWORD,
};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{ids::StoreEpoch, postgres_schema};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_maintenance_commands_require_fencing_and_preserve_epoch() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let receipt = postgres_schema::initialize(&fixture.request(), &trust, deadline()).unwrap();
    let epoch = receipt.store_epoch;
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("connection");
    let uri = |user: &str| {
        format!(
            "postgresql://{user}:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        )
    };
    fs::write(&connection, uri("td_probe")).unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let invoke = |operation: &str, selected: StoreEpoch, extra: &[&str], code| {
        let encoded = selected.to_string();
        let mut args = vec![
            "postgres",
            operation,
            "--connection-file",
            connection.to_str().unwrap(),
            "--epoch",
            &encoded,
        ];
        args.extend_from_slice(extra);
        command_output(&args, fixture.root.path(), code)
    };
    let check = |text: &str, blocked| {
        let parsed = td_json::parse(text).unwrap();
        assert_eq!(
            parsed.get("store_epoch").and_then(td_json::Json::as_str),
            Some(epoch.to_string().as_str())
        );
        assert_eq!(
            parsed.get("blocked").and_then(td_json::Json::as_bool),
            Some(blocked)
        );
        assert_eq!(
            parsed
                .get("recovery_verified")
                .and_then(td_json::Json::as_bool),
            Some(false)
        );
    };
    check(&invoke("maintenance-inspect", epoch, &[], 0), false);
    assert!(invoke("maintenance-enter", epoch, &[], 2).contains("invalid-arguments"));
    let fenced = ["--instances-and-smtp-fenced"];
    assert!(invoke(
        "maintenance-enter",
        StoreEpoch::from_bytes([2; 16]),
        &fenced,
        1
    )
    .contains("epoch-mismatch"));
    for extra in [
        vec!["--instances-and-smtp-fenced", "--instances-and-smtp-fenced"],
        vec!["--instances-and-smtp-fenced", "true"],
        vec!["--force"],
        vec![
            "--instances-and-smtp-fenced",
            "--epoch",
            "22222222222222222222222222222222",
        ],
    ] {
        assert!(invoke("maintenance-enter", epoch, &extra, 2).contains("invalid-arguments"));
    }
    assert!(invoke("maintenance-inspect", epoch, &fenced, 2).contains("invalid-arguments"));
    assert_eq!(
        sql(&fixture, "SELECT blocked FROM td_mta.runtime_barrier"),
        "f"
    );
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    fs::write(&connection, uri("mail_runtime")).unwrap();
    assert!(invoke("maintenance-enter", epoch, &fenced, 1).contains("owner-required"));
    assert_eq!(
        sql(&fixture, "SELECT blocked FROM td_mta.runtime_barrier"),
        "f"
    );
    fs::write(&connection, uri("td_probe")).unwrap();
    check(&invoke("maintenance-enter", epoch, &fenced, 0), true);
    check(&invoke("maintenance-enter", epoch, &fenced, 0), true);
    check(&invoke("maintenance-inspect", epoch, &[], 0), true);
    assert_eq!(
        postgres_schema::inspect(&fixture.request(), &trust, deadline()).unwrap(),
        receipt
    );
    sql(&fixture, "DELETE FROM td_mta.runtime_barrier");
    assert!(invoke("maintenance-enter", epoch, &fenced, 1).contains("barrier-integrity"));
    assert!(invoke("maintenance-inspect", epoch, &[], 1).contains("barrier-integrity"));
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.runtime_barrier"),
        "0"
    );
}
