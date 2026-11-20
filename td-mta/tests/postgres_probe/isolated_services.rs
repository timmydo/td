use super::{command_output, deadline, sql, Fixture, ProbeChild};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};
use td_mta::{
    config::profile::Roles, ids::StoreEpoch, postgres_configuration as config,
    postgres_configuration_store as versions, postgres_probe::Request,
    postgres_runtime_access::ServiceRole, postgres_selection as selection,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &td_crypto::TrustStore,
    epoch: StoreEpoch,
    shared: &str,
    desktop: &str,
    owner: &Path,
) {
    let smtp = TcpListener::bind("127.0.0.1:0").unwrap();
    let https = TcpListener::bind("127.0.0.1:0").unwrap();
    let smtp_addr = smtp.local_addr().unwrap();
    let https_addr = https.local_addr().unwrap();
    let outbound = TcpListener::bind("127.0.0.1:0").unwrap();
    let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
    let frontend = desktop
        .replace("127.0.0.1:53", &dns.local_addr().unwrap().to_string())
        .replace(
            "port = 465",
            &format!("port = {}", outbound.local_addr().unwrap().port()),
        )
        .replace("127.0.0.1:443", &https_addr.to_string())
        .replace(
            "https://localhost",
            &format!("https://localhost:{}", https_addr.port()),
        );
    let receive = format!("{}[listener \"smtp\"]\nkind = \"direct_smtp\"\nbind = \"{smtp_addr}\"\nserver_name = \"localhost\"\ncertificate = \"public\"\nsession_limit = 2\nper_peer_limit = 2\n", frontend.split_once("[listener").unwrap().0);
    let relay = frontend.split_once("[certificate").unwrap().0;
    let migadu = relay.replace("personal-smarthost", "other-smarthost");
    let shared = shared.replace(
        "username = \"user\"",
        "username = \"user\"\noutbound_profile = \"migadu\"",
    );
    let profiles = [
        ("frontend", 2, frontend.as_str()),
        ("receive", 1, receive.as_str()),
        ("relay", 4, relay),
        ("migadu", 4, migadu.as_str()),
    ]
    .map(|(name, bits, source)| config::Profile {
        name,
        roles: Roles::from_bits(bits).unwrap(),
        source: source.as_bytes(),
    });
    let candidate = config::prepare(
        request,
        trust,
        deadline(),
        epoch,
        shared.as_bytes(),
        &profiles,
    )
    .unwrap();
    let version = versions::Version::from_bytes([92; 16]);
    versions::publish(request, trust, deadline(), version, &candidate).unwrap();
    selection::publish(
        request,
        trust,
        deadline(),
        selection::prepare(epoch, 1, version, &candidate).unwrap(),
    )
    .unwrap();
    let mut statements = String::from("REVOKE TEMP ON DATABASE postgres FROM PUBLIC;");
    for &role in ServiceRole::ALL {
        let login = role.login();
        statements.push_str(&format!("CREATE ROLE {login} LOGIN NOINHERIT PASSWORD 'disposable-postgres-fixture-password';GRANT USAGE ON SCHEMA td_mta TO {login};"));
    }
    sql(fixture, &statements);
    command_output(
        &[
            "postgres",
            "service-material-bind",
            "--connection-file",
            owner.to_str().unwrap(),
            "--epoch",
            &epoch.to_string(),
        ],
        fixture.root.path(),
        0,
    );
    let connection = |role: ServiceRole| fixture.root.join(role.login());
    let template = fs::read_to_string(owner).unwrap();
    for &role in ServiceRole::ALL {
        let login = role.login();
        let mut grants = String::new();
        for (table, privileges) in role.grants() {
            grants.push_str(&format!("GRANT {privileges} ON td_mta.{table} TO {login};"));
        }
        if role != ServiceRole::Acme {
            grants.push_str(&format!("GRANT USAGE ON SCHEMA td_mta_service TO {login};GRANT SELECT ON td_mta_service.materials,td_mta_service.material_chunks,td_mta_service.active_certificates TO {login};"));
        }
        sql(fixture, &grants);
        let file = connection(role);
        fs::write(&file, template.replace("td_probe:", &format!("{login}:"))).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        command_output(
            &[
                "postgres",
                "runtime-check",
                "--connection-file",
                file.to_str().unwrap(),
            ],
            fixture.root.path(),
            0,
        );
    }
    super::isolated_grants::rejects_drift(
        fixture,
        &Request {
            user: ServiceRole::Receive.login(),
            mode: td_mta::postgres_probe::Mode::Runtime,
            ..*request
        },
        trust,
    );
    super::acme::publish_fixture_pair_as(
        fixture,
        Request {
            user: ServiceRole::Acme.login(),
            mode: td_mta::postgres_probe::Mode::Runtime,
            ..*request
        },
    );
    let peer = super::serving_relay::Relay::accepting(fixture, outbound, dns, "receive");
    command_output(
        &[
            "acme-status",
            "--connection-file",
            connection(ServiceRole::Acme).to_str().unwrap(),
            "--certificate-profile",
            "public",
        ],
        fixture.root.path(),
        0,
    );
    drop((smtp, https));
    let local = td_fs::TempDir::new("m").unwrap();
    let mut servers = Vec::new();
    for &role in &[
        ServiceRole::Receive,
        ServiceRole::Frontend,
        ServiceRole::Relay,
        ServiceRole::Migadu,
    ] {
        let private = local.join(role.name());
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let output = private.join("log");
        let runtime = private.join("run");
        let scratch = private.join("scratch");
        for path in [&runtime, &scratch] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut child = ProbeChild(
            Command::new(env!("CARGO_BIN_EXE_td-mta"))
                .arg("serve")
                .arg("--connection-file")
                .arg(connection(role))
                .args(["--profile", role.name(), "--runtime"])
                .arg(&runtime)
                .arg("--scratch")
                .arg(&scratch)
                .stdout(fs::File::create(&output).unwrap())
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let message = fs::read_to_string(&output).unwrap();
            if message.contains("\"status\":\"ready\"") {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none() && Instant::now() < until,
                "{role:?}: {message}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        servers.push(child);
    }
    let mut mail = BufReader::new(TcpStream::connect(smtp_addr).unwrap());
    mail.get_mut()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    for (send, code) in [("","220"),("EHLO sender.test\r\n","250"),("MAIL FROM:<sender@remote.test>\r\n","250"),("RCPT TO:<main@example.test>\r\n","250"),("DATA\r\n","354"),("From: sender@remote.test\r\nTo: main@example.test\r\nSubject: isolation\r\n\r\nhello\r\n.\r\n","250"),("QUIT\r\n","221")] {
        mail.get_mut().write_all(send.as_bytes()).unwrap();
        loop {
            let mut line = String::new();
            mail.read_line(&mut line).unwrap();
            assert!(line.starts_with(code),"{line}");
            if line.as_bytes().get(3) == Some(&b' ') {break;}
        }
    }
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
    let account = td_mta::ids::AccountId::from_bytes([0xaa; 16]);
    let device =
        td_mta::postgres_devices::prepare_creation(epoch, account, "isolated client", 1).unwrap();
    td_mta::postgres_devices::publish_creation(request, trust, deadline(), &device).unwrap();
    let credentials = [b"user:".as_slice(), device.password()].concat();
    let pem = super::certificate::pem("BASIC", &credentials);
    let basic = std::str::from_utf8(&pem)
        .unwrap()
        .lines()
        .filter(|s| !s.starts_with("---"))
        .collect::<String>();
    let http = |method: &str, path: &str, body: &str| {
        let (mut tls, _) = super::smtp_transport::upgrade_protocol(
            TcpStream::connect(https_addr).unwrap(),
            Arc::new(td_mta::clock::RuntimeClock::new()),
            &fixture.ca,
            td_crypto::TlsProtocol::Http1,
        );
        write!(tls, "{method} {path} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {basic}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", https_addr.port(), body.len()).unwrap();
        tls.flush().unwrap();
        let mut reply = String::new();
        tls.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        reply
    };
    let reply = http("GET", "/.well-known/jmap", "");
    assert!(reply.contains("urn:ietf:params:jmap:mail"), "{reply}");
    let session = td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
    let source_account = &session["accounts"].as_obj().unwrap().first().unwrap().0;
    let body = format!(
        r#"{{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/query",{{"accountId":"{source_account}"}},"received"]]}}"#
    );
    let reply = http("POST", "/jmap/api", &body);
    let result = td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
    let response = &result["methodResponses"].as_arr().unwrap()[0]
        .as_arr()
        .unwrap();
    assert_eq!(response[0].as_str(), Some("Email/query"), "{reply}");
    assert_eq!(response[1]["ids"].as_arr().unwrap().len(), 1);

    // The SMTP peer witnesses delivery. Check the actual login at the two
    // transactions that take responsibility for the submission and its outcome.
    sql(fixture, "CREATE FUNCTION public.require_writer() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF session_user<>TG_ARGV[0] THEN RAISE EXCEPTION 'unexpected service writer';
        END IF; RETURN NEW; END $$;
        CREATE TRIGGER frontend_enqueue BEFORE INSERT ON td_mta.submissions FOR EACH ROW
            EXECUTE FUNCTION public.require_writer('td_mta_frontend');
        CREATE TRIGGER migadu_accept BEFORE INSERT ON td_mta.attempt_acceptance_receipts FOR EACH ROW
            EXECUTE FUNCTION public.require_writer('td_mta_migadu')");
    let submitted = super::multi_account::submit_draft(
        account,
        td_mta::ids::IdentityId::from_bytes([0xbb; 16]),
        "main@example.test",
        |body| http("POST", "/jmap/api", body),
    );
    let id = td_mta::wire::Id::parse(&submitted)
        .unwrap()
        .local::<td_mta::ids::SubmissionId>()
        .unwrap();
    let accepted = format!("SELECT count(*) FROM td_mta.submission_routes s JOIN td_mta.recipients r
        USING (account,submission_id) JOIN td_mta.attempt_acceptance_receipts a USING (account,attempt_id)
        WHERE s.submission_id=decode('{id}','hex') AND s.route_name='other-smarthost' AND r.state=4");
    let until = Instant::now() + Duration::from_secs(30);
    while peer.accepted.load(std::sync::atomic::Ordering::Acquire) == 0
        || sql(fixture, &accepted) != "1"
    {
        assert!(
            Instant::now() < until,
            "timed out waiting for accepted submission {id}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(peer.accepted.load(std::sync::atomic::Ordering::Acquire), 1);
}
