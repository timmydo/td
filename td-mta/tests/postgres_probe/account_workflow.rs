use super::{command_output, deadline, sql, Fixture};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use td_mta::{
    account_configuration::Edit,
    ids::{AccountId, StoreEpoch},
    postgres_account_admin as admin,
    postgres_probe::Request,
    postgres_selection as selection,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &td_crypto::TrustStore,
    epoch: StoreEpoch,
    connection: &Path,
) {
    let root = fixture.root.path();
    let operation = root.join("create-account.operation");
    let args = [
        "postgres",
        "account-create",
        "--connection-file",
        connection.to_str().unwrap(),
        "--address",
        "second@example.test",
        "--name",
        "Second Account",
        "--operation-file",
        operation.to_str().unwrap(),
    ];
    let output = fs::File::create(root.join("umask-output")).unwrap();
    let mut child = super::ProbeChild(
        std::process::Command::new("sh")
            .arg("-c")
            .arg("umask 0400; exec \"$@\"")
            .arg("account-umask-fixture")
            .arg(env!("CARGO_BIN_EXE_td-mta"))
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let stop = deadline() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < stop);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let result = fs::read_to_string(root.join("umask-output")).unwrap();
    let result = td_json::parse(&result).unwrap();
    let account = AccountId::parse(result.get("account").unwrap().as_str().unwrap()).unwrap();
    assert!(result.get("restart_required").unwrap().as_bool().unwrap());
    assert_eq!(
        fs::metadata(&operation).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let bytes = fs::read(&operation).unwrap();
    let retained = admin::Operation::decode(&bytes).unwrap();
    assert_eq!(retained.account(), account);
    assert_eq!(retained.encode().unwrap(), bytes);
    for len in [0, 1, bytes.len() - 1] {
        assert!(admin::Operation::decode(&bytes[..len]).is_err());
    }
    let mut changed = bytes.clone();
    changed[50] ^= 1;
    assert!(admin::Operation::decode(&changed).is_err());
    assert!(command_output(&args, root, 1).contains("already-exists"));
    for command in ["account-apply", "account-operation-inspect"] {
        assert!(command_output(
            &[
                "postgres",
                command,
                "--connection-file",
                connection.to_str().unwrap(),
                "--operation-file",
                operation.to_str().unwrap()
            ],
            root,
            0
        )
        .contains("\"generation\":2"));
    }
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.accounts"), "2");
    let (_, candidate) = selection::read(request, trust, deadline(), epoch)
        .unwrap()
        .unwrap();
    for index in 0..candidate.profile_count() {
        assert!(
            std::str::from_utf8(candidate.profile(index).unwrap().source)
                .unwrap()
                .contains("postmaster_account = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"")
        );
    }
    let listing = command_output(
        &[
            "postgres",
            "account-list",
            "--connection-file",
            connection.to_str().unwrap(),
        ],
        root,
        0,
    );
    td_json::parse(&listing).unwrap();
    assert!(listing.contains("second@example.test"));
    assert!(listing.contains("Second Account"));
    // Competing retained edits cannot clobber one another.
    let first = admin::prepare(
        request,
        trust,
        deadline(),
        epoch,
        &Edit::Update {
            account,
            name: Some("Renamed"),
            username: None,
        },
    )
    .unwrap();
    let second = admin::prepare(
        request,
        trust,
        deadline(),
        epoch,
        &Edit::Update {
            account,
            name: None,
            username: Some("different"),
        },
    )
    .unwrap();
    let receipt = admin::apply(request, trust, deadline(), &first).unwrap();
    assert_eq!(receipt.generation, 3);
    assert_eq!(
        admin::apply(request, trust, deadline(), &second),
        Err(admin::Error::Selection(selection::Error::Conflict))
    );
    assert_eq!(
        admin::apply(request, trust, deadline(), &retained)
            .unwrap()
            .generation,
        2
    );
    let added = root.join("alias.operation");
    let account_text = account.to_string();
    command_output(
        &[
            "postgres",
            "account-alias-add",
            "--connection-file",
            connection.to_str().unwrap(),
            "--account",
            &account_text,
            "--address",
            "alias@example.test",
            "--operation-file",
            added.to_str().unwrap(),
        ],
        root,
        0,
    );
    let before = selection::read(request, trust, deadline(), epoch)
        .unwrap()
        .unwrap();
    assert_eq!(before.0.generation, 4);
    let removed = root.join("remove.operation");
    command_output(
        &[
            "postgres",
            "account-alias-remove",
            "--connection-file",
            connection.to_str().unwrap(),
            "--account",
            &account_text,
            "--address",
            "alias@EXAMPLE.test",
            "--operation-file",
            removed.to_str().unwrap(),
        ],
        root,
        0,
    );
    let after = selection::read(request, trust, deadline(), epoch)
        .unwrap()
        .unwrap();
    assert!(!std::str::from_utf8(after.1.shared())
        .unwrap()
        .contains("alias@example.test"));
    assert!(std::str::from_utf8(after.1.shared())
        .unwrap()
        .contains("second@example.test"));
    // A lost selection reply is resolved from the same retained request.
    let rename = admin::prepare(
        request,
        trust,
        deadline(),
        epoch,
        &Edit::Update {
            account,
            name: Some("Final"),
            username: None,
        },
    )
    .unwrap();
    sql(fixture,"CREATE FUNCTION public.admin_delay_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(12); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER admin_delay AFTER INSERT ON td_mta.config_selection_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.admin_delay_commit()");
    let result = admin::apply(
        request,
        trust,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
        &rename,
    );
    assert_eq!(
        result,
        Err(admin::Error::Selection(selection::Error::Indeterminate))
    );
    let stop = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while sql(
        fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(std::time::Instant::now() < stop);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    sql(
        fixture,
        "DROP TRIGGER admin_delay ON td_mta.config_selection_receipts",
    );
    assert_eq!(
        admin::apply(request, trust, deadline(), &rename)
            .unwrap()
            .generation,
        6
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.accounts"), "2");
    fs::set_permissions(&operation, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(command_output(
        &[
            "postgres",
            "account-apply",
            "--connection-file",
            connection.to_str().unwrap(),
            "--operation-file",
            operation.to_str().unwrap()
        ],
        root,
        1
    )
    .contains("read-sync-or-integrity-failed"));
}
