use super::{deadline, sql, Fixture, ProbeChild, PASSWORD};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use td_crypto::Digest;
use td_mta::{
    config::profile::Roles,
    ids::{AccountId, BlobId, EmailId, StoreEpoch},
    postgres_configuration::{self as configuration, Profile},
    postgres_configuration_store as versions,
    postgres_materials::{self as materials, Kind, Version},
    postgres_selection as selection,
};

trait Exchange: Read + Write {}
impl<T: Read + Write> Exchange for T {}

pub(super) fn exercise(
    fixture: &Fixture,
    epoch: StoreEpoch,
    account: AccountId,
    email: EmailId,
    blob: BlobId,
    size: u64,
    digest: [u8; 32],
) {
    let trust = td_crypto::TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let chain = fs::read(fixture.root.join("server.crt")).unwrap();
    let key = fs::read(fixture.root.join("server.key")).unwrap();
    for (n, kind, bytes) in [
        (1, Kind::CertificateChain, chain.as_slice()),
        (2, Kind::PrivateKey, key.as_slice()),
        (3, Kind::TrustBundle, fixture.ca.as_slice()),
        (4, Kind::RelayPassword, b"unused-fixture".as_slice()),
    ] {
        materials::insert(
            &owner,
            &trust,
            deadline(),
            epoch,
            Version::from_bytes([n; 16]),
            kind,
            bytes,
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let shared = format!("version = 1\n[account \"{account}\"]\nusername = \"replica-user\"\n[domain \"example.test\"]\nmx_host = \"localhost\"\n[alias \"main@example.test\"]\naccount = \"{account}\"\n[identity \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"]\naccount = \"{account}\"\nemail = \"main@example.test\"\n");
    let source = format!(
        r#"[server]
hostname = "localhost"
jmap_origin = "https://localhost:{port}"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
route = "unused"
host = "localhost"
port = 465
username = "unused"
password_file = "/postgres-material/{password}"
ca_file = "/postgres-material/{ca}"
[certificate "public"]
mode = "files"
chain_file = "/postgres-material/{chain}"
key_file = "/postgres-material/{key}"
[listener "https"]
kind = "https"
bind = "{address}"
certificate = "public"
"#,
        port = address.port(),
        password = Version::from_bytes([4; 16]),
        ca = Version::from_bytes([3; 16]),
        chain = Version::from_bytes([1; 16]),
        key = Version::from_bytes([2; 16])
    );
    let public = source.replace(
        &format!("bind = \"{address}\""),
        &format!("bind = \"0.0.0.0:{}\"", address.port()),
    );
    let relay = source.split("[certificate").next().unwrap();
    let socket_directory = fixture.root.join("jmap");
    fs::create_dir(&socket_directory).unwrap();
    fs::set_permissions(&socket_directory, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = socket_directory.join("socket");
    let local = format!(
        "{}[listener \"local\"]\nkind = \"local_jmap\"\nbind = \"{}\"\n",
        relay
            .replace("route = \"unused\"", "route = \"unix-unused\"")
            .replace(
                &format!("https://localhost:{}", address.port()),
                "http://localhost"
            )
            .replace(
                &format!(
                    "ca_file = \"/postgres-material/{}\"\n",
                    Version::from_bytes([3; 16])
                ),
                ""
            ),
        socket.display()
    );
    let profiles = [
        Profile {
            name: "local",
            roles: Roles::from_bits(2).unwrap(),
            source: local.as_bytes(),
        },
        Profile {
            name: "replica",
            roles: Roles::from_bits(2).unwrap(),
            source: source.as_bytes(),
        },
        Profile {
            name: "public",
            roles: Roles::from_bits(2).unwrap(),
            source: public.as_bytes(),
        },
        Profile {
            name: "relay",
            roles: Roles::from_bits(4).unwrap(),
            source: relay.as_bytes(),
        },
    ];
    let candidate = configuration::prepare(
        &owner,
        &trust,
        deadline(),
        epoch,
        shared.as_bytes(),
        &profiles,
    )
    .unwrap();
    let version = versions::Version::from_bytes([9; 16]);
    versions::publish(&owner, &trust, deadline(), version, &candidate).unwrap();
    selection::publish(
        &owner,
        &trust,
        deadline(),
        selection::prepare(epoch, 0, version, &candidate).unwrap(),
    )
    .unwrap();
    let device =
        td_mta::postgres_devices::prepare_creation(epoch, account, "local fixture", 1).unwrap();
    td_mta::postgres_devices::publish_creation(&owner, &trust, deadline(), &device).unwrap();
    sql(fixture,"CREATE ROLE mail_reader LOGIN PASSWORD 'disposable-postgres-fixture-password'; GRANT USAGE ON SCHEMA td_mta TO mail_reader; GRANT SELECT ON ALL TABLES IN SCHEMA td_mta TO mail_reader; GRANT UPDATE(singleton) ON td_mta.config_selection TO mail_reader; GRANT UPDATE(id) ON td_mta.accounts TO mail_reader; GRANT INSERT ON td_mta.leases TO mail_reader; GRANT INSERT,UPDATE,DELETE ON td_mta.application_logs,td_mta.application_log_state TO mail_reader");
    let ca = fixture.root.join("local-read-ca");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("local-read-connection");
    fs::write(
        &connection,
        format!(
            "postgresql://mail_reader:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = fixture.root.join("reader-runtime");
    let scratch = fixture.root.join("reader-scratch");
    for path in [&runtime, &scratch] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let command = |profile: &str, read_only: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        cmd.args(["serve", "--connection-file"])
            .arg(&connection)
            .args(["--profile", profile, "--runtime"])
            .arg(&runtime)
            .arg("--scratch")
            .arg(&scratch);
        if read_only {
            cmd.arg("--read-only");
        }
        cmd
    };
    for (profile, read_only, reason) in [
        ("replica", false, "runtime-qualification"),
        ("public", true, "replica-local-jmap-only"),
        ("relay", true, "profile"),
    ] {
        let mut arguments = vec![
            "serve",
            "--connection-file",
            connection.to_str().unwrap(),
            "--profile",
            profile,
            "--runtime",
            runtime.to_str().unwrap(),
            "--scratch",
            scratch.to_str().unwrap(),
        ];
        if read_only {
            arguments.push("--read-only");
        }
        let result = super::command_output_with_timeout(
            &arguments,
            &fixture.root,
            1,
            Duration::from_secs(90),
        );
        assert!(result.contains(reason), "{result}");
    }
    drop(listener);
    for profile in ["replica", "local"] {
        let output = fixture.root.join("reader-output");
        let mut child = ProbeChild(
            command(profile, true)
                .stdout(fs::File::create(&output).unwrap())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            let log = fs::read_to_string(&output).unwrap();
            if log.contains("\"status\":\"ready\"") {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "reader exited: {log}"
            );
            assert!(Instant::now() < until, "reader readiness: {log}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let encoded = super::certificate::pem(
            "BASIC",
            &[b"replica-user:".as_slice(), device.password()].concat(),
        );
        let authorization = std::str::from_utf8(&encoded)
            .unwrap()
            .lines()
            .filter(|s| !s.starts_with("---"))
            .collect::<String>();
        if profile == "local" {
            for (headers, expected) in [
                ("Host: localhost\r\n", "HTTP/1.1 401"),
                ("Host: elsewhere.test\r\n", "HTTP/1.1 421"),
                (
                    "Host: localhost\r\nOrigin: https://localhost\r\n",
                    "HTTP/1.1 403",
                ),
            ] {
                let mut stream = UnixStream::connect(&socket).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                write!(
                    stream,
                    "GET /.well-known/jmap HTTP/1.1\r\n{headers}Connection: close\r\n\r\n"
                )
                .unwrap();
                assert!(super::serving::persistent_reply(&mut stream)
                    .0
                    .starts_with(expected));
            }
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            for connection in ["keep-alive", "close"] {
                write!(stream, "GET http://localhost/.well-known/jmap HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {authorization}\r\nConnection: {connection}\r\n\r\n").unwrap();
                assert!(super::serving::persistent_reply(&mut stream)
                    .0
                    .starts_with("HTTP/1.1 200"));
            }
        }
        let exchange = |method: &str, path: &str, body: &[u8]| {
            let (mut tls, host): (Box<dyn Exchange>, String) = if profile == "local" {
                let stream = UnixStream::connect(&socket).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                (Box::new(stream), "localhost".into())
            } else {
                let (tls, _) = super::smtp_transport::upgrade_protocol(
                    TcpStream::connect(address).unwrap(),
                    Arc::new(td_mta::clock::RuntimeClock::new()),
                    &fixture.ca,
                    td_crypto::TlsProtocol::Http1,
                );
                (Box::new(tls), format!("localhost:{}", address.port()))
            };
            write!(tls,"{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {authorization}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            tls.write_all(body).unwrap();
            tls.flush().unwrap();
            super::serving::persistent_reply(&mut tls)
        };
        let (head, bytes) = exchange("GET", "/.well-known/jmap", b"");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let session = td_json::parse_slice(&bytes).unwrap();
        let account_wire = format!("a{account}");
        let email_wire = format!("e{email}");
        assert_eq!(
            session["accounts"][account_wire.as_str()]["isReadOnly"].as_bool(),
            Some(true)
        );
        assert!(session["capabilities"]
            .get("urn:ietf:params:jmap:submission")
            .is_none());
        let batch = |calls: td_json::Json| {
            td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":calls}).to_string()
        };
        let read = batch(
            td_json::json!([["Mailbox/get",{"accountId":account_wire},"folders"],["Email/get",{"accountId":account_wire,"ids":[email_wire],"properties":["id","subject","keywords"]},"mail"]]),
        );
        let (head, bytes) = exchange("POST", "/jmap/api", read.as_bytes());
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let reply = td_json::parse_slice(&bytes).unwrap();
        assert_eq!(reply["methodResponses"][1][0].as_str(), Some("Email/get"));
        let rights = &reply["methodResponses"][0][1]["list"][0]["myRights"];
        assert_eq!(rights["mayReadItems"].as_bool(), Some(true));
        for name in [
            "mayAddItems",
            "mayRemoveItems",
            "maySetSeen",
            "maySetKeywords",
            "mayCreateChild",
            "mayRename",
            "mayDelete",
            "maySubmit",
        ] {
            assert_eq!(rights[name].as_bool(), Some(false), "{name}");
        }
        let before = sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        );
        for calls in [
            td_json::json!([["Email/set",{"accountId":account_wire,"destroy":[email_wire]},"write"]]),
            td_json::json!([["Email/get",{"accountId":account_wire,"ids":[email_wire]},"read"],["Email/set",{"accountId":account_wire,"destroy":[email_wire]},"write"]]),
            td_json::json!([["Mailbox/set",{"accountId":account_wire,"create":{"new":{"name":"refused"}}},"folder"]]),
        ] {
            assert!(exchange("POST", "/jmap/api", batch(calls).as_bytes())
                .0
                .starts_with("HTTP/1.1 403"));
        }
        assert!(
            exchange("POST", &format!("/jmap/upload/{account_wire}"), b"refused")
                .0
                .starts_with("HTTP/1.1 403")
        );
        let (head, body) = exchange(
            "GET",
            &format!("/jmap/download/{account_wire}/b{blob}/message.eml?type=message/rfc822"),
            b"",
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body.len() as u64, size);
        let mut hash = td_crypto::Sha256::try_new().unwrap();
        hash.update(&body).unwrap();
        assert_eq!(hash.finish().unwrap(), digest);
        assert_eq!(
            sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts"
            ),
            before
        );
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "0");
        let stop = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["stop", "--runtime"])
            .arg(&runtime)
            .output()
            .unwrap();
        assert!(stop.status.success());
        super::wait_command(&mut child);
        if profile == "local" {
            assert!(!socket.exists());
        }
    }
    assert_ne!(
        sql(fixture, "SELECT count(*) FROM td_mta.application_logs"),
        "0"
    );
}
