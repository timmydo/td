use super::{deadline, sql, Fixture, ProbeChild, PASSWORD};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use td_mta::{
    config::profile::Roles,
    generations::GenerationSet,
    ids::{AccountId, IdentityId, StoreEpoch},
    postgres_configuration::{self as configuration, Profile},
    postgres_configuration_store as versions, postgres_runtime_profile as runtime,
    postgres_selection as selection,
    tls_policy::TlsPolicies,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &td_crypto::TrustStore,
    epoch: StoreEpoch,
    shared: &str,
    frontend: &str,
) {
    let relay_socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_dns = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let frontend = frontend
        .replace(
            "port = 465",
            &format!("port = {}", relay_socket.local_addr().unwrap().port()),
        )
        .replace("127.0.0.1:53", &relay_dns.local_addr().unwrap().to_string());
    let a = AccountId::from_bytes([0xaa; 16]);
    let b = AccountId::from_bytes([0xcc; 16]);
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let smtp_socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let smtp_address = smtp_socket.local_addr().unwrap();
    let shared = format!("{shared}\n[account \"cccccccccccccccccccccccccccccccc\"]\nusername = \"user-b\"\n[alias \"second@example.test\"]\naccount = \"cccccccccccccccccccccccccccccccc\"\n[identity \"dddddddddddddddddddddddddddddddd\"]\naccount = \"cccccccccccccccccccccccccccccccc\"\nemail = \"second@example.test\"\n");
    let shared = format!("{shared}\n[alias \"alias@example.test\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n");
    let frontend = frontend
        .replace(
            "[server]",
            "[server]\npostmaster_account = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
        )
        .replace(
            "https://localhost",
            &format!("https://localhost:{}", address.port()),
        )
        .replace("bind = \"127.0.0.1:443\"", &format!("bind = \"{address}\""));
    let frontend = format!("{frontend}\n[listener \"smtp\"]\nkind = \"direct_smtp\"\nbind = \"{smtp_address}\"\nserver_name = \"localhost\"\ncertificate = \"public\"\nsession_limit = 1\nper_peer_limit = 1\n");
    let candidate = configuration::prepare(
        request,
        trust,
        deadline(),
        epoch,
        shared.as_bytes(),
        &[Profile {
            name: "multi",
            roles: Roles::from_bits(7).unwrap(),
            source: frontend.as_bytes(),
        }],
    )
    .unwrap();
    let version = versions::Version::from_bytes([61; 16]);
    versions::publish(request, trust, deadline(), version, &candidate).unwrap();
    selection::publish(
        request,
        trust,
        deadline(),
        selection::prepare(epoch, 1, version, &candidate).unwrap(),
    )
    .unwrap();
    let mut domain = GenerationSet::<TlsPolicies>::at_startup();
    assert!(matches!(
        runtime::load(
            request,
            trust,
            deadline(),
            epoch,
            "multi",
            domain.reserve().unwrap()
        ),
        Err(runtime::Error::Bootstrap(
            td_mta::postgres_accounts::Error::Missing
        ))
    ));
    td_mta::postgres_accounts::publish(
        request,
        trust,
        deadline(),
        td_mta::postgres_accounts::prepare(epoch, b).unwrap(),
    )
    .unwrap();
    let prepared = runtime::load(
        request,
        trust,
        deadline(),
        epoch,
        "multi",
        domain.reserve().unwrap(),
    )
    .unwrap();
    assert_eq!(prepared.bindings().count(), 2);
    for (account, own, other) in [
        (
            a,
            IdentityId::from_bytes([0xbb; 16]),
            IdentityId::from_bytes([0xdd; 16]),
        ),
        (
            b,
            IdentityId::from_bytes([0xdd; 16]),
            IdentityId::from_bytes([0xbb; 16]),
        ),
    ] {
        let binding = prepared.binding_for(account).unwrap();
        assert!(binding.identity_email(own).is_some());
        assert_eq!(binding.identity_email(other), None);
        let catalog = binding.identity_catalog().unwrap();
        assert_eq!(catalog.account(), account);
        assert_eq!(catalog.ids().collect::<Vec<_>>(), vec![own]);
        assert!(catalog.object(other).unwrap().is_none());
    }
    assert!(prepared
        .binding_for(AccountId::from_bytes([1; 16]))
        .is_none());
    let (published, retired) = prepared.publish(&mut domain).unwrap();
    drop(retired);
    assert_eq!(published.bindings().count(), 2);
    super::multi_receiving::exercise(fixture, request, trust, &published);
    // Exercise the complete account ceiling with a small DATA reservation.
    let mut expanded = shared.clone();
    for index in 1..=14u8 {
        let account = AccountId::from_bytes([index; 16]);
        expanded.push_str(&format!("\n[account \"{account}\"]\nusername = \"extra{index}\"\n[alias \"extra{index}@example.test\"]\naccount = \"{account}\"\n"));
        td_mta::postgres_accounts::publish(
            request,
            trust,
            deadline(),
            td_mta::postgres_accounts::prepare(epoch, account).unwrap(),
        )
        .unwrap();
    }
    let frontend = format!("{frontend}\n[limits]\nmessage_bytes = 1048576\n");
    let candidate = configuration::prepare(
        request,
        trust,
        deadline(),
        epoch,
        expanded.as_bytes(),
        &[Profile {
            name: "multi",
            roles: Roles::from_bits(7).unwrap(),
            source: frontend.as_bytes(),
        }],
    )
    .unwrap();
    let version = versions::Version::from_bytes([62; 16]);
    versions::publish(request, trust, deadline(), version, &candidate).unwrap();
    selection::publish(
        request,
        trust,
        deadline(),
        selection::prepare(
            epoch,
            published.binding().selection().generation,
            version,
            &candidate,
        )
        .unwrap(),
    )
    .unwrap();
    let prepared = runtime::load(
        request,
        trust,
        deadline(),
        epoch,
        "multi",
        domain.reserve().unwrap(),
    )
    .unwrap();
    assert_eq!(prepared.bindings().count(), 16);
    let (published, retired) = prepared.publish(&mut domain).unwrap();
    drop(retired);
    use td_mta::postgres_route_owners as owners;
    let (held, grant) = super::multi_relay::leases(fixture, request, trust, &published);
    let held_live = std::cell::Cell::new(true);
    let relay = super::serving_relay::Relay::multi(fixture, relay_socket, relay_dns);
    super::create_runtime_login(fixture);
    super::grant_runtime_mail(fixture);
    let devices: Vec<_> = [a, b]
        .into_iter()
        .map(|account| {
            let device =
                td_mta::postgres_devices::prepare_creation(epoch, account, "multi-account", 1)
                    .unwrap();
            td_mta::postgres_devices::publish_creation(request, trust, deadline(), &device)
                .unwrap();
            device
        })
        .collect();
    let local = td_fs::TempDir::new("ma").unwrap();
    fs::set_permissions(local.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = local.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = local.join("connection");
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
    let runtime = local.join("r");
    let scratch = local.join("s");
    for dir in [&runtime, &scratch] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    drop(socket);
    drop(smtp_socket);
    let output = local.join("output");
    let mut child = ProbeChild(
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["serve", "--connection-file"])
            .arg(&connection)
            .args(["--profile", "multi", "--runtime"])
            .arg(&runtime)
            .arg("--scratch")
            .arg(&scratch)
            .stdout(fs::File::create(&output).unwrap())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let stop = Instant::now() + Duration::from_secs(20);
    loop {
        let status = fs::read_to_string(&output).unwrap();
        if status.contains("\"status\":\"ready\"") {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "service exited: {status}"
        );
        assert!(Instant::now() < stop, "service startup timed out: {status}");
        thread::sleep(Duration::from_millis(20));
    }
    super::multi_receiving::smtp(fixture, smtp_address, false);
    let basic = |name: &str, password: &[u8]| {
        let input = [name.as_bytes(), b":", password].concat();
        let encoded = super::certificate::pem("BASIC", &input);
        std::str::from_utf8(&encoded)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("---"))
            .collect::<String>()
    };
    let query = |authorization: &str,
                 method: &str,
                 target: &str,
                 content_type: &str,
                 body: &str| {
        if held_live.get() {
            owners::renew(request, trust, deadline(), &held, grant.generation).unwrap();
        }
        let stop = Instant::now() + Duration::from_secs(10);
        loop {
            let (mut tls, _) = super::smtp_transport::upgrade_protocol(
                TcpStream::connect(address).unwrap(),
                Arc::new(td_mta::clock::RuntimeClock::new()),
                &fixture.ca,
                td_crypto::TlsProtocol::Http1,
            );
            write!(tls, "{method} {target} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {authorization}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", address.port(),body.len()).unwrap();
            tls.flush().unwrap();
            let mut reply = String::new();
            tls.read_to_string(&mut reply).unwrap();
            if !reply.starts_with("HTTP/1.1 429") {
                break reply;
            }
            assert!(Instant::now() < stop);
            thread::sleep(Duration::from_millis(100));
        }
    };
    for (index, name, own, other, email) in [
        (0, "user", a, b, "main@example.test"),
        (1, "user-b", b, a, "second@example.test"),
    ] {
        let authorization = basic(name, devices[index].password());
        let mut own_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let own = td_mta::wire::encode_id(own, &mut own_bytes).unwrap();
        let mut other_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let other = td_mta::wire::encode_id(other, &mut other_bytes).unwrap();
        let session = query(&authorization, "GET", "/.well-known/jmap", "text/plain", "");
        assert!(session.starts_with("HTTP/1.1 200"), "{session}");
        assert!(session.contains(own));
        assert!(!session.contains(other));
        let body=td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[["Identity/get",{"accountId":own},"0"],["Mailbox/get",{"accountId":other},"1"]]}).to_string();
        let reply = query(
            &authorization,
            "POST",
            "/jmap/api",
            "application/json",
            &body,
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        let value = td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            value["methodResponses"][0][1]["list"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            value["methodResponses"][0][1]["list"][0]["email"].as_str(),
            Some(email)
        );
        assert_eq!(
            value["methodResponses"][1][1]["type"].as_str(),
            Some("accountNotFound")
        );
        let refused = query(
            &authorization,
            "POST",
            &format!("/jmap/upload/{other}"),
            "text/plain",
            "bad",
        );
        assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
        let accepted = query(
            &authorization,
            "POST",
            &format!("/jmap/upload/{own}"),
            "text/plain",
            "own",
        );
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
        let wrong = query(
            &basic(name, devices[1 - index].password()),
            "GET",
            "/.well-known/jmap",
            "text/plain",
            "",
        );
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
    }
    super::multi_receiving::smtp(fixture, smtp_address, true);
    let queue_command = |name: &str| {
        let result = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args([name, "--runtime"])
            .arg(&runtime)
            .output()
            .unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap()
    };
    assert!(queue_command("queue-route-repair").contains("requested"));
    // A competing owner holds A while B must still make forward progress.
    for (index, name, account, identity, email) in [
        (
            0,
            "user",
            a,
            IdentityId::from_bytes([0xbb; 16]),
            "main@example.test",
        ),
        (
            1,
            "user-b",
            b,
            IdentityId::from_bytes([0xdd; 16]),
            "second@example.test",
        ),
    ] {
        let authorization = basic(name, devices[index].password());
        let mut account_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let account = td_mta::wire::encode_id(account, &mut account_bytes).unwrap();
        let mut identity_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let identity = td_mta::wire::encode_id(identity, &mut identity_bytes).unwrap();
        let mailboxes = query(&authorization, "POST", "/jmap/api", "application/json",
            &td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Mailbox/get",{"accountId":account},"m"]]}).to_string());
        let mailboxes = td_json::parse(mailboxes.split_once("\r\n\r\n").unwrap().1).unwrap();
        let mailbox = mailboxes["methodResponses"][0][1]["list"][0]["id"]
            .as_str()
            .unwrap();
        let body = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[
            ["Email/set",{"accountId":account,"create":{"draft":{
                "mailboxIds":{mailbox:true},"keywords":{"$draft":true,"$seen":true},"from":[{"email":email}],"to":[{"email":"recipient@example.test"}],"subject":"Account relay","messageId":["multi-relay@example.test"],"sentAt":"2026-10-13T12:00:59Z","textBody":[{"partId":"text","type":"text/plain"}],"bodyValues":{"text":{"value":"Private sender.","isEncodingProblem":false,"isTruncated":false}}
            }}},"0"],
            ["EmailSubmission/set",{"accountId":account,"create":{"submission":{"emailId":"#draft","identityId":identity}}},"1"]
        ]}).to_string();
        let reply = query(
            &authorization,
            "POST",
            "/jmap/api",
            "application/json",
            &body,
        );
        let value = td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(
            value["methodResponses"][1][1]["created"]["submission"]["id"]
                .as_str()
                .is_some(),
            "{reply}"
        );
    }
    let stop = Instant::now() + Duration::from_secs(30);
    while sql(fixture, "SELECT count(*) FROM td_mta.recipients WHERE account=decode(repeat('cc',16),'hex') AND state=4") != "1" {
        assert!(Instant::now() < stop);
        owners::renew(request, trust, deadline(), &held, grant.generation).unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.recipients WHERE account=decode(repeat('aa',16),'hex') AND state=4"), "0");
    assert!(queue_command("queue-route-status").contains("requested"));
    owners::release(request, trust, deadline(), &held, grant.generation).unwrap();
    held_live.set(false);
    let stop = Instant::now() + Duration::from_secs(30);
    while sql(
        fixture,
        "SELECT count(*) FROM td_mta.recipients WHERE state=4",
    ) != "2"
    {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 2);
    let stop = Instant::now() + Duration::from_secs(30);
    while !queue_command("queue-route-status").contains("repaired") {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(100));
    }
    // Repeated idle scheduling renews leases without manufacturing claim receipts.
    let receipts = sql(fixture, "SELECT count(*) FROM td_mta.route_owner_receipts");
    thread::sleep(Duration::from_secs(6));
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.route_owner_receipts"),
        receipts
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&runtime)
        .output()
        .unwrap();
    assert!(stopped.status.success());
    super::wait_command(&mut child);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.devices WHERE revoked_at IS NULL"
        ),
        "2"
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools required"]
fn real_postgres_multiple_account_sessions_are_isolated() {
    super::exercise_configuration(super::ConfigurationExercise::MultiAccount);
}
