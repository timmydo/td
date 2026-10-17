use super::{certificate, deadline, sql, Fixture, ProbeChild, PASSWORD};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::{mpsc, Arc},
    time::Duration,
};

pub(super) fn exercise(fixture: &Fixture, smtp: TcpListener, https: TcpListener) {
    let smtp_address = smtp.local_addr().unwrap();
    let https_address = https.local_addr().unwrap();
    let connection = fixture.root.join("serving-connection");
    let ca = fixture.root.join("serving-ca.pem");
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&ca, &fixture.ca).unwrap();
    fs::write(
        &connection,
        format!(
            "postgresql://mail_runtime:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let local = td_fs::TempDir::new("m").unwrap();
    let start = |profile: &str| {
        let runtime = local.join(if profile == "cloud" { "c" } else { "d" });
        let scratch = local.join(if profile == "cloud" { "cs" } else { "ds" });
        for path in [&runtime, &scratch] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut child = ProbeChild(
            Command::new(env!("CARGO_BIN_EXE_td-mta"))
                .args(["serve", "--connection-file"])
                .arg(&connection)
                .args(["--profile", profile, "--runtime"])
                .arg(&runtime)
                .arg("--scratch")
                .arg(&scratch)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = send.send(line);
        });
        let line = receive.recv_timeout(Duration::from_secs(20)).unwrap();
        reader.join().unwrap();
        assert!(line.contains("\"status\":\"ready\""), "{line}");
        (child, runtime)
    };
    drop((smtp, https));
    let (mut cloud, cloud_runtime) = start("cloud");
    let (mut desktop, desktop_runtime) = start("desktop");
    let mut smtp = BufReader::new(TcpStream::connect(smtp_address).unwrap());
    smtp.get_mut()
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut exchange = |command: &[u8], code: &str| {
        smtp.get_mut().write_all(command).unwrap();
        loop {
            let mut line = String::new();
            smtp.read_line(&mut line).unwrap();
            assert!(line.starts_with(code), "{line}");
            if line.as_bytes().get(3) == Some(&b' ') {
                break;
            }
        }
    };
    exchange(b"", "220");
    exchange(b"EHLO sender.example.test\r\n", "250");
    exchange(b"MAIL FROM:<sender@example.test>\r\n", "250");
    exchange(b"RCPT TO:<main@example.test>\r\n", "250");
    exchange(b"DATA\r\n", "354");
    exchange(
        b"Subject: Executable PostgreSQL\r\n\r\nDelivered by the real process.\r\n.\r\n",
        "250",
    );
    exchange(b"QUIT\r\n", "221");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
    let trust = td_crypto::TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = td_mta::postgres_schema::inspect(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = td_mta::ids::AccountId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let device =
        td_mta::postgres_devices::prepare_creation(epoch, account, "process client", 1).unwrap();
    td_mta::postgres_devices::publish_creation(&fixture.request(), &trust, deadline(), &device)
        .unwrap();
    let credentials = [b"user:".as_slice(), device.password()].concat();
    let encoded = certificate::pem("BASIC", &credentials);
    let encoded = std::str::from_utf8(&encoded)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("---"))
        .collect::<String>();
    let query = |authorization: &str| {
        let (mut tls, _) = super::smtp_transport::upgrade_protocol(
            TcpStream::connect(https_address).unwrap(),
            Arc::new(td_mta::clock::RuntimeClock::new()),
            &fixture.ca,
            td_crypto::TlsProtocol::Http1,
        );
        write!(tls,"GET /.well-known/jmap HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nConnection: close\r\n\r\n",https_address.port()).unwrap();
        tls.flush().unwrap();
        let mut reply = String::new();
        tls.read_to_string(&mut reply).unwrap();
        reply
    };
    let reply = query(&encoded);
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("urn:ietf:params:jmap:mail"), "{reply}");
    let revoke =
        td_mta::postgres_devices::prepare_revocation(epoch, account, device.device_id(), 2)
            .unwrap();
    td_mta::postgres_devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoke)
        .unwrap();
    assert!(query(&encoded).starts_with("HTTP/1.1 401"));
    for (child, runtime) in [(&mut cloud, cloud_runtime), (&mut desktop, desktop_runtime)] {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["stop", "--runtime"])
            .arg(runtime)
            .output()
            .unwrap();
        assert!(output.status.success());
        super::wait_command(child);
    }
    use super::termination_sender::{send, Signal};
    for (profile, signal) in [("cloud", Signal::Terminate), ("desktop", Signal::Interrupt)] {
        let (mut child, _) = start(profile);
        send(&child.0, signal).unwrap();
        super::wait_command(&mut child);
    }
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
}
