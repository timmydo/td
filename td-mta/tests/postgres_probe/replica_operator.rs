use super::{
    command_output_with_timeout, create_runtime_login, deadline, grant_runtime_mail, sql, Fixture,
    PASSWORD,
};
use std::{fs, os::unix::fs::PermissionsExt, time::Duration};
use td_crypto::TrustStore;
use td_mta::{
    postgres_replica_source::{self, Source},
    postgres_schema,
    replica_source::Binding,
};

pub(super) fn exercise(origin: &str, source_ca: &[u8], password: &[u8], binding: Binding) {
    let fixture = Fixture::new();
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let source = Source::new(origin, binding.epoch).unwrap();
    postgres_replica_source::initialize(&fixture.request(), &trust, deadline(), epoch, &source)
        .unwrap();
    let ca = fixture.root.join("local-ca.pem");
    let remote_ca = fixture.root.join("source-ca.pem");
    let secret = fixture.root.join("source-password");
    fs::write(&ca, &fixture.ca).unwrap();
    fs::write(&remote_ca, source_ca).unwrap();
    fs::write(&secret, [password, b"\r\n"].concat()).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let connection = |user: &str| {
        let path = fixture.root.join(format!("{user}-connection"));
        fs::write(
            &path,
            format!(
                "postgresql://{user}:{}@localhost:{}/postgres?sslrootcert={}\n",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port,
                ca.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    };
    let discovered = command_output_with_timeout(
        &[
            "postgres",
            "replica-discover",
            "--source-origin",
            origin,
            "--username",
            "user",
            "--password-file",
            secret.to_str().unwrap(),
            "--ca-file",
            remote_ca.to_str().unwrap(),
        ],
        fixture.root.path(),
        0,
        Duration::from_secs(75),
    );
    let discovered: td_json::Json = td_json::parse(&discovered).unwrap();
    assert_eq!(
        discovered.get("account").and_then(td_json::Json::as_str),
        Some(binding.account.to_string().as_str())
    );
    assert_eq!(
        discovered
            .get("source_epoch")
            .and_then(td_json::Json::as_str),
        Some(binding.epoch.to_string().as_str())
    );
    assert_eq!(
        discovered
            .get("source_origin")
            .and_then(td_json::Json::as_str),
        Some(origin)
    );
    let owner = connection("td_probe");
    let runtime = connection("mail_runtime");
    let invoke = |operation: &str, connection: &std::path::Path, extras: &[&str], status| {
        let epoch = epoch.to_string();
        let account = binding.account.to_string();
        let mut args = vec![
            "postgres",
            operation,
            "--connection-file",
            connection.to_str().unwrap(),
            "--epoch",
            &epoch,
            "--account",
            &account,
        ];
        args.extend_from_slice(extras);
        command_output_with_timeout(&args, fixture.root.path(), status, Duration::from_secs(75))
    };
    let source_epoch = binding.epoch.to_string();
    let mut remote = vec![
        "--source-origin",
        origin,
        "--source-epoch",
        &source_epoch,
        "--username",
        "user",
        "--password-file",
        secret.to_str().unwrap(),
        "--ca-file",
        remote_ca.to_str().unwrap(),
    ];
    let mut seed = remote.clone();
    seed.extend([
        "--body-bytes",
        "1048576",
        "--blobs",
        "100",
        "--emails",
        "100",
        "--receipts",
        "100",
    ]);
    assert!(invoke("replica-progress", &owner, &[], 1).contains("account-not-initialized"));
    assert!(invoke("replica-account-init", &owner, &seed, 0).contains("\"email_count\":0"));
    let before = sql(
        &fixture,
        "SELECT encode(seed_digest,'hex') FROM td_mta.replica_accounts",
    );
    assert!(invoke("replica-account-init", &owner, &seed, 0).contains("\"unfinished_ids\":1"));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(seed_digest,'hex') FROM td_mta.replica_accounts"
        ),
        before
    );
    assert!(invoke("replica-copy", &owner, &remote, 1).contains("publication-failed"));
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    sql(&fixture,"GRANT INSERT,UPDATE,DELETE ON td_mta.replica_progress,td_mta.replica_pending,td_mta.replica_mailbox_receipts,td_mta.replica_email_receipts,td_mta.replica_blob_identities TO mail_runtime");
    // Fill a socket-backed stdout before starting the copy, so its first
    // report blocks independently of host speed or pipe capacity.
    let (mut output, _consumer) = std::os::unix::net::UnixStream::pair().unwrap();
    output.set_nonblocking(true).unwrap();
    let chunk = [0u8; 65536];
    let mut filled = 0;
    loop {
        use std::io::Write;
        match output.write(&chunk) {
            Ok(count) => {
                assert!(count != 0);
                filled += count;
                assert!(filled <= 16 * 1024 * 1024);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("stdout fixture: {error}"),
        }
    }
    output.set_nonblocking(false).unwrap();
    let output: std::os::fd::OwnedFd = output.into();
    let mut blocked = super::ProbeChild(
        std::process::Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .env_clear()
            .args([
                "postgres",
                "replica-copy",
                "--connection-file",
                runtime.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &binding.account.to_string(),
            ])
            .args(&remote)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(output))
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stop = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        assert!(
            blocked.0.try_wait().unwrap().is_none(),
            "stdout must remain blocked"
        );
        if sql(&fixture, "SELECT count(*) FROM td_mta.emails") == "1"
            && sql(
                &fixture,
                "SELECT count(*) FROM pg_stat_activity WHERE usename='mail_runtime'",
            ) == "0"
        {
            break;
        }
        assert!(
            std::time::Instant::now() < stop,
            "blocked output retained the copy session"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(blocked);
    assert_eq!(
        sql(
            &fixture,
            "SELECT get_byte(done,0) FROM td_mta.replica_pending"
        ),
        "1"
    );
    let state = fixture.root.join("follow-state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let start_follow = || {
        super::ProbeChild(
            std::process::Command::new(env!("CARGO_BIN_EXE_td-mta"))
                .env_clear()
                .args([
                    "postgres",
                    "replica-follow",
                    "--connection-file",
                    runtime.to_str().unwrap(),
                    "--epoch",
                    &epoch.to_string(),
                    "--account",
                    &binding.account.to_string(),
                    "--state-dir",
                    state.to_str().unwrap(),
                    "--idle-seconds",
                    "1",
                ])
                .args(&remote)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let observe = |phase: &str| {
        let stop = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok(text) = fs::read_to_string(state.join("state.json")) {
                let value = td_json::parse(&text).unwrap();
                if value.get("phase").and_then(td_json::Json::as_str) == Some(phase) {
                    return value;
                }
            }
            assert!(
                std::time::Instant::now() < stop,
                "follow observation: {phase}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let status = |expected| {
        command_output_with_timeout(
            &[
                "postgres",
                "replica-follow-status",
                "--state-dir",
                state.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &binding.account.to_string(),
            ],
            fixture.root.path(),
            expected,
            Duration::from_secs(15),
        )
    };
    let mut follow = start_follow();
    observe("idle");
    assert!(status(0).contains("\"recent_success\":true"));
    let mut duplicate = start_follow();
    let duplicate_end = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = duplicate.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < duplicate_end,
            "duplicate follow must refuse its held lock"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let original = fs::read(&secret).unwrap();
    fs::write(&secret, b"wrong-private-proof-password").unwrap();
    let failed = observe("backoff");
    assert!(failed
        .get("error")
        .and_then(td_json::Json::as_str)
        .is_some());
    assert!(status(1).contains("\"recent_success\":false"));
    fs::write(&secret, original).unwrap();
    observe("idle");
    super::termination_sender::send(&follow.0, super::termination_sender::Signal::Terminate)
        .unwrap();
    super::wait_command(&mut follow);
    observe("stopped");
    assert!(status(1).contains("\"phase\":\"stopped\""));
    let mut follow = start_follow();
    observe("idle");
    super::termination_sender::send(&follow.0, super::termination_sender::Signal::Interrupt)
        .unwrap();
    super::wait_command(&mut follow);
    observe("stopped");
    remote.extend(["--steps", "4"]);
    let output = invoke("replica-copy", &runtime, &remote, 0);
    assert!(output.contains("\"applied\":0"));
    assert!(output.contains("\"empty_email_changes\":true"));
    assert!(!output.contains(std::str::from_utf8(password).unwrap()));
    let progress = invoke("replica-progress", &owner, &[], 0);
    assert!(progress.contains("\"email_count\":1"));
    assert!(progress.contains("\"inventory_complete\":true"));
    assert!(progress.contains("\"copy_complete\":false"));
    assert!(invoke("replica-account-init", &owner, &seed, 0).contains("\"email_count\":1"));
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "1");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(invoke("replica-copy", &runtime, &remote, 1).contains("secret-file"));
}
