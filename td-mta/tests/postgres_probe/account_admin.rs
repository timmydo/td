use super::{command_output, deadline, sql, Fixture, PASSWORD};
use std::{fs, os::unix::fs::PermissionsExt};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_account_commands_preserve_bootstrap_receipts() {
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
    let invoke = |command: &str, selected_epoch: StoreEpoch, selected: AccountId, status| {
        command_output(
            &[
                "postgres",
                command,
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &selected_epoch.to_string(),
                "--account",
                &selected.to_string(),
            ],
            fixture.root.path(),
            status,
        )
    };
    assert!(invoke("account-inspect", epoch, account, 1).contains("bootstrap-receipt-not-found"));
    assert!(
        invoke("account-init", StoreEpoch::from_bytes([0; 16]), account, 1)
            .contains("epoch-mismatch")
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.accounts"), "0");
    let output = invoke("account-init", epoch, account, 0);
    let parsed = td_json::parse(&output).unwrap();
    let inbox = parsed.get("inbox").and_then(td_json::Json::as_str).unwrap();
    let inspected = invoke("account-inspect", epoch, account, 0);
    let parsed = td_json::parse(&inspected).unwrap();
    assert_eq!(
        parsed.get("inbox").and_then(td_json::Json::as_str),
        Some(inbox)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex'),encode(floor,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001|0000000000000000"
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.account_usage"),
        "1"
    );
    assert_eq!(
        sql(&fixture, "SELECT encode(id,'hex') FROM td_mta.mailboxes"),
        inbox
    );
    assert_eq!(sql(&fixture,"SELECT encode(sequence,'hex'),operation,kind,action,encode(object,'hex') FROM td_mta.changes"),format!("0000000000000001|1|1|1|{inbox}"));
    // Later mailbox state must not be reset by either administrative command.
    sql(&fixture,"UPDATE td_mta.accounts SET sequence=decode('0000000000000009','hex'); UPDATE td_mta.mailboxes SET name='Renamed'");
    assert!(invoke("account-init", epoch, account, 1).contains("bootstrap-conflict"));
    let inspected = invoke("account-inspect", epoch, account, 0);
    assert!(inspected.contains(inbox));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000009"
    );
    assert_eq!(
        sql(&fixture, "SELECT name FROM td_mta.mailboxes"),
        "Renamed"
    );
    let unreceipted = AccountId::from_bytes([2; 16]);
    sql(&fixture,"INSERT INTO td_mta.accounts(id,sequence,floor) VALUES(decode(repeat('02',16),'hex'),decode('0000000000000001','hex'),decode('0000000000000000','hex'))");
    assert!(invoke("account-init", epoch, unreceipted, 1).contains("unreceipted-account-exists"));
    assert!(
        invoke("account-inspect", epoch, unreceipted, 1).contains("bootstrap-receipt-not-found")
    );
    let refused = command_output(
        &[
            "postgres",
            "account-init",
            "--connection-file",
            "not-for-output",
            "--epoch",
            &epoch.to_string(),
            "--account",
            &account.to_string(),
            "--account",
            &account.to_string(),
        ],
        fixture.root.path(),
        2,
    );
    assert!(refused.contains("invalid-arguments"));
    assert!(!refused.contains("not-for-output"));
    sql(&fixture,"CREATE SEQUENCE public.bootstrap_attempts; CREATE FUNCTION public.delay_bootstrap() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('public.bootstrap_attempts'); PERFORM pg_sleep(11); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_bootstrap AFTER INSERT ON td_mta.account_bootstraps DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_bootstrap()");
    let delayed = AccountId::from_bytes([3; 16]);
    let uncertain = invoke("account-init", epoch, delayed, 1);
    assert!(uncertain.contains("indeterminate-inspect-account"));
    let parsed = td_json::parse(&uncertain).unwrap();
    let attempted_inbox = parsed.get("inbox").and_then(td_json::Json::as_str).unwrap();
    sql(
        &fixture,
        "DROP TRIGGER delay_bootstrap ON td_mta.account_bootstraps",
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT last_value,is_called FROM public.bootstrap_attempts"
        ),
        "1|t"
    );
    let stored=sql(&fixture,"SELECT encode(inbox,'hex') FROM td_mta.account_bootstraps WHERE account=decode(repeat('03',16),'hex')");
    if stored.is_empty() {
        assert!(
            invoke("account-inspect", epoch, delayed, 1).contains("bootstrap-receipt-not-found")
        );
        invoke("account-init", epoch, delayed, 0);
    } else {
        assert_eq!(stored, attempted_inbox);
        assert!(invoke("account-inspect", epoch, delayed, 0).contains(attempted_inbox));
        assert!(invoke("account-init", epoch, delayed, 1).contains("bootstrap-conflict"));
    }
    assert_eq!(sql(&fixture,"SELECT count(*) FROM td_mta.account_bootstraps WHERE account=decode(repeat('03',16),'hex')"),"1");
}
