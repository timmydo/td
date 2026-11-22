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
    uncertain_upload(fixture, &published, [(a, &devices[0]), (b, &devices[1])]);
    let (held, grant) = super::multi_relay::leases(fixture, request, trust, &published);
    let fill_health = |account: AccountId| {
        sql(fixture, &format!("DELETE FROM td_mta.route_health WHERE account=decode('{account}','hex') AND failure_count=0"));
        sql(fixture, &format!("INSERT INTO td_mta.route_health SELECT decode('{account}','hex'),'held-'||i,decode(repeat('00',32),'hex'),decode('0000000000000001','hex'),1,4,0,300000,decode('0000000000000001','hex') FROM generate_series(1,1024) i"));
    };
    let release_health = |account: AccountId| {
        sql(fixture, &format!("UPDATE td_mta.route_health SET repair_generation=NULL WHERE account=decode('{account}','hex') AND route_name LIKE 'held-%'"));
    };
    let held_live = std::cell::Cell::new(true);
    let relay = super::serving_relay::Relay::accepting(fixture, relay_socket, relay_dns, "multi");
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
    let start_worker = || {
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
        child
    };
    let mut child = start_worker();
    let stop_worker = |child: &mut ProbeChild, custody_outstanding: bool| {
        let stopped = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["stop", "--runtime"])
            .arg(&runtime)
            .output()
            .unwrap();
        assert!(stopped.status.success());
        if custody_outstanding {
            let end = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert_eq!(status.code(), Some(1));
                    break;
                }
                assert!(Instant::now() < end, "worker did not stop");
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            super::wait_command(child);
        }
    };
    super::multi_receiving::smtp(
        fixture,
        smtp_address,
        false,
        published.binding().quotas().body_bytes,
    );
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
    super::multi_receiving::smtp(
        fixture,
        smtp_address,
        true,
        published.binding().quotas().body_bytes,
    );
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
    let submit =
        |index: usize, name: &str, account: AccountId, identity: IdentityId, email: &str| {
            let authorization = basic(name, devices[index].password());
            submit_draft(account, identity, email, |body| {
                query(
                    &authorization,
                    "POST",
                    "/jmap/api",
                    "application/json",
                    body,
                )
            })
        };
    let submit_a = || {
        submit(
            0,
            "user",
            a,
            IdentityId::from_bytes([0xbb; 16]),
            "main@example.test",
        )
    };
    let submit_b = || {
        submit(
            1,
            "user-b",
            b,
            IdentityId::from_bytes([0xdd; 16]),
            "second@example.test",
        )
    };
    submit_a();
    submit_b();
    let wait_for = |query: &str, expected: &str| {
        let stop = Instant::now() + Duration::from_secs(45);
        while sql(fixture, query) != expected {
            assert!(
                Instant::now() < stop,
                "timed out waiting for {query} = {expected}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    };
    let b_claim = "SELECT count(*) FROM td_mta.route_owners WHERE account=decode(repeat('cc',16),'hex') AND owner_boot IS NOT NULL";
    let a_done = "SELECT count(*) FROM td_mta.recipients WHERE account=decode(repeat('aa',16),'hex') AND state=4";
    let b_done = "SELECT count(*) FROM td_mta.recipients WHERE account=decode(repeat('cc',16),'hex') AND state=4";
    let b_attempts = "SELECT count(*) FROM td_mta.submission_attempts a JOIN td_mta.recipients r ON a.account=r.account AND a.submission_id=r.submission_id WHERE a.account=decode(repeat('cc',16),'hex') AND r.state<>4";
    // A competing owner holds A while B must still make forward progress.
    let stop = Instant::now() + Duration::from_secs(30);
    while sql(fixture, b_done) != "1" {
        assert!(Instant::now() < stop);
        owners::renew(request, trust, deadline(), &held, grant.generation).unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(sql(fixture, a_done), "0");
    assert!(queue_command("queue-route-status").contains("requested"));
    owners::release(request, trust, deadline(), &held, grant.generation).unwrap();
    held_live.set(false);
    wait_for(a_done, "1");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 2);
    let stop = Instant::now() + Duration::from_secs(30);
    while !queue_command("queue-route-status").contains("repaired") {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(100));
    }
    stop_worker(&mut child, false);

    // A full ledger must leave B queued while A delivers. The idle owner
    // cleanup rolls back on capacity, so its row marks every retry up to
    // the successful claim. A sequence preserves timing across rollbacks.
    sql(fixture, "INSERT INTO td_mta.route_owner_receipts SELECT decode(repeat('cc',16),'hex'),decode(md5(i::text),'hex'),decode(repeat('00',32),'hex'),int8send(1),0,9000000000000000000 FROM generate_series(1,1024-(SELECT count(*)::int FROM td_mta.route_owner_receipts WHERE account=decode(repeat('cc',16),'hex'))) i");
    sql(fixture, "INSERT INTO td_mta.route_owners SELECT account,'capacity-probe',destination,generation,NULL,NULL,0,NULL FROM td_mta.route_owners WHERE account=decode(repeat('cc',16),'hex');
        CREATE SEQUENCE public.claim_time MINVALUE 0 START WITH 0;
        CREATE FUNCTION public.time_claim() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE previous bigint; observed bigint;
        BEGIN
            SELECT last_value INTO previous FROM public.claim_time;
            observed := floor(extract(epoch FROM clock_timestamp())*1000);
            IF previous>0 AND observed-previous<4900 THEN RAISE EXCEPTION 'early owner-capacity retry'; END IF;
            PERFORM setval('public.claim_time',observed); RETURN OLD;
        END $$;
        CREATE TRIGGER time_claim BEFORE DELETE ON td_mta.route_owners FOR EACH ROW WHEN (OLD.route_name='capacity-probe') EXECUTE FUNCTION public.time_claim();
        GRANT SELECT,UPDATE ON SEQUENCE public.claim_time TO mail_runtime");
    child = start_worker();
    submit_a();
    submit_b();
    wait_for(a_done, "2");
    wait_for("SELECT is_called FROM public.claim_time", "t");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 3);
    assert_eq!(sql(fixture, b_attempts), "0");
    assert_eq!(sql(fixture, b_claim), "0");
    sql(fixture, "DELETE FROM td_mta.route_owner_receipts WHERE account=decode(repeat('cc',16),'hex') AND operation=decode(md5('1'),'hex')");
    wait_for(b_done, "2");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 4);
    stop_worker(&mut child, false);

    // B can acquire its route, but cannot reserve a health row before SMTP.
    sql(fixture, "DELETE FROM td_mta.route_owner_receipts WHERE account=decode(repeat('cc',16),'hex') AND claimed_at=0");
    fill_health(b);
    child = start_worker();
    wait_for(b_claim, "1");
    submit_b();
    submit_a();
    wait_for(a_done, "3");
    assert_eq!(sql(fixture, b_attempts), "0");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 5);
    // Keep 256 latched destinations and four repair receipts at each. Releasing
    // state capacity alone must not permit SMTP before receipt capacity returns.
    sql(fixture, "BEGIN; DELETE FROM td_mta.route_health WHERE account=decode(repeat('cc',16),'hex') AND route_name LIKE 'held-%' AND substring(route_name FROM 6)::integer>256; INSERT INTO td_mta.route_health_receipts SELECT h.account,decode(md5(h.route_name||':'||i),'hex'),h.destination,h.route_name,h.destination,2,int8send(i::bigint),int8send(i::bigint+1),h.failure_count,h.reason,h.failed_at,h.next_at,NULL FROM td_mta.route_health h CROSS JOIN generate_series(1,4) i WHERE h.account=decode(repeat('cc',16),'hex') AND h.route_name LIKE 'held-%'; COMMIT");
    submit_a();
    wait_for(a_done, "4");
    assert_eq!(sql(fixture, b_attempts), "0");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 6);
    release_health(b);
    wait_for(b_done, "3");
    wait_for("SELECT count(*) FROM td_mta.route_health_receipts WHERE account=decode(repeat('cc',16),'hex') AND action=3", "1");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 7);
    // Stop once acceptance commits while the health reset is still outstanding.
    // A replacement owner must deliver the next submission without replay.
    sql(fixture, "CREATE FUNCTION public.hold_reset() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=3 THEN RAISE EXCEPTION 'fixture holds reset'; END IF; RETURN NEW; END $$; CREATE TRIGGER hold_reset BEFORE INSERT ON td_mta.route_health_receipts FOR EACH ROW EXECUTE FUNCTION public.hold_reset()");
    submit_b();
    wait_for(b_done, "4");
    stop_worker(&mut child, true);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.attempt_acceptance_receipts f WHERE account=decode(repeat('cc',16),'hex') AND NOT EXISTS(SELECT 1 FROM td_mta.route_health_receipts h WHERE h.account=f.account AND h.operation=f.attempt_id AND h.action=3)"), "1");
    sql(
        fixture,
        "DROP TRIGGER hold_reset ON td_mta.route_health_receipts",
    );
    child = start_worker();
    submit_b();
    wait_for(b_done, "5");
    wait_for("SELECT count(*) FROM td_mta.route_health_receipts WHERE account=decode(repeat('cc',16),'hex') AND action=3", "1");
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 9);
    let cancel_b = |id: &str| {
        let mut account_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let account = td_mta::wire::encode_id(b, &mut account_bytes).unwrap();
        let body = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],"methodCalls":[["EmailSubmission/set",{"accountId":account,"update":{id:{"undoStatus":"canceled"}}},"cancel"]]});
        let reply = query(
            &basic("user-b", devices[1].password()),
            "POST",
            "/jmap/api",
            "application/json",
            &body.to_string(),
        );
        let reply = td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(
            reply["methodResponses"][0][1]["updated"].get(id).is_some(),
            "{reply}"
        );
    };
    cancel_during_body_verification(fixture, submit_b, cancel_b, wait_for);
    let cancel_at_reply = |refuse: &std::sync::atomic::AtomicBool| {
        use std::sync::atomic::Ordering::Release;
        relay.cancel_release.store(false, Release);
        relay.cancel_waiting.store(false, Release);
        refuse.store(true, Release);
        let canceled = submit_b();
        let stop = Instant::now() + Duration::from_secs(30);
        while !relay
            .cancel_waiting
            .load(std::sync::atomic::Ordering::Acquire)
        {
            assert!(Instant::now() < stop);
            thread::sleep(Duration::from_millis(10));
        }
        cancel_b(&canceled);
        relay
            .cancel_release
            .store(true, std::sync::atomic::Ordering::Release);
    };
    cancel_at_reply(&relay.refuse_mail);
    wait_for("SELECT count(*) FROM td_mta.submission_attempts WHERE account=decode(repeat('cc',16),'hex') AND active", "0");
    cancel_at_reply(&relay.refuse_auth);
    wait_for("SELECT count(*) FROM td_mta.route_health_receipts h JOIN td_mta.submission_attempts a ON a.account=h.account AND a.attempt_id=h.operation JOIN td_mta.recipients r ON r.account=a.account AND r.attempt_id=a.attempt_id WHERE h.account=decode(repeat('cc',16),'hex') AND h.action=1 AND h.reason=5 AND h.repair_generation IS NOT NULL AND NOT a.active AND r.state=6", "1");
    submit_a();
    wait_for(a_done, "5");
    assert_eq!(
        relay.accepted.load(std::sync::atomic::Ordering::Acquire),
        10
    );
    // Repeated idle scheduling renews leases without manufacturing claim receipts.
    let receipts = sql(fixture, "SELECT count(*) FROM td_mta.route_owner_receipts");
    thread::sleep(Duration::from_secs(6));
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.route_owner_receipts"),
        receipts
    );
    stop_worker(&mut child, false);
    // A removed route will never be claimed again. Its dead owner's
    // canceled custody must drain during another route's ordinary upkeep.
    sql(fixture, "UPDATE td_mta.submission_attempts a SET route_name='removed',active=true FROM td_mta.route_health_receipts h WHERE a.account=h.account AND a.attempt_id=h.operation AND h.action=1 AND h.reason=5; INSERT INTO td_mta.route_owners SELECT account,route_name,destination,generation,owner_boot,owner_operation,0,1 FROM td_mta.submission_attempts WHERE route_name='removed' AND active");
    sql(fixture, "DELETE FROM td_mta.route_owners WHERE account=decode(repeat('cc',16),'hex') AND owner_boot IS NULL; INSERT INTO td_mta.route_owners SELECT account,'recent-'||i,destination,generation,NULL,NULL,(extract(epoch FROM clock_timestamp())*1000)::bigint,NULL FROM td_mta.route_owners CROSS JOIN generate_series(1,1023) i WHERE route_name='removed'");
    let claim = owners::prepare(
        published.binding_for(b).unwrap(),
        td_mta::ids::BootId::from_bytes([93; 16]),
    )
    .unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    td_mta::postgres_submission_retention::Step::AbandonedOwners
        .run(
            request,
            trust,
            deadline(),
            &claim,
            grant.generation,
            grant.claimed_at,
        )
        .unwrap();
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submission_attempts WHERE route_name='removed' AND NOT active"), "1");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.route_owners WHERE route_name='removed'"
        ),
        "0"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.devices WHERE revoked_at IS NULL"
        ),
        "2"
    );
}

// Pause the body snapshot after Prepared commits. Frontend cancellation
// must retain custody until the worker sees it at fresh body admission.
fn cancel_during_body_verification(
    fixture: &Fixture,
    submit: impl Fn() -> String,
    cancel: impl Fn(&str),
    wait: impl Fn(&str, &str),
) {
    let mut blocker = ProbeChild(
        super::sql_command(fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = blocker.0.stdin.take().unwrap();
    writeln!(input, "SELECT pg_advisory_lock(82419);").unwrap();
    wait(
        "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND objid=82419 AND granted",
        "1",
    );
    sql(fixture, "CREATE FUNCTION public.hold_prepared() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(82419); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER hold_prepared AFTER INSERT ON td_mta.submission_attempts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.hold_prepared()");
    let submission = submit();
    wait(
        "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND objid=82419 AND NOT granted",
        "1",
    );
    writeln!(input, "BEGIN; LOCK TABLE td_mta.blob_chunks IN ACCESS EXCLUSIVE MODE; SELECT pg_advisory_unlock(82419);").unwrap();
    wait("SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE l.relation='td_mta.blob_chunks'::regclass AND NOT l.granted AND a.query LIKE 'SELECT a.sequence,a.floor,b.length%'", "1");
    cancel(&submission);
    let retained = "SELECT count(*) FROM td_mta.submission_attempts a JOIN td_mta.recipients r ON a.account=r.account AND a.attempt_id=r.attempt_id WHERE a.account=decode(repeat('cc',16),'hex') AND r.state=6 AND a.active";
    assert_eq!(sql(fixture, retained), "1");
    writeln!(input, "COMMIT;").unwrap();
    drop(input);
    super::wait_command(&mut blocker);
    wait(retained, "0");
    sql(fixture, "DROP TRIGGER hold_prepared ON td_mta.submission_attempts; DROP FUNCTION public.hold_prepared()");
}

// Account A's deferred COMMIT keeps its row locked after the HTTP worker has
// returned uncertainty. Account B uses the very same process-wide upload slot.
fn uncertain_upload(
    fixture: &Fixture,
    profile: &runtime::PublishedProfile,
    accounts: [(AccountId, &td_mta::postgres_devices::Creation); 2],
) {
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
        postgres_bootstrap::Bootstrap,
        postgres_http_uploads as uploads,
        postgres_input::Loaded,
        postgres_read::ReadSlots,
        postgres_uploads,
    };
    let loaded = Loaded {
        bootstrap: Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            "multi",
        )
        .unwrap(),
        trust: td_crypto::TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let clock = RuntimeClock::new();
    let slots = ReadSlots::new(1).unwrap();
    let pending = uploads::Pending::default();
    let publish = |(account, device): (AccountId, &td_mta::postgres_devices::Creation), ms| {
        let cap = Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
        let mut meter = Meter::new(
            cap,
            Charge {
                io_bytes: 1024 * 1024,
                records: 4096,
                output_bytes: 1024,
                ..Default::default()
            },
        );
        uploads::Uploads {
            loaded: &loaded,
            executable: std::path::Path::new(env!("CARGO_BIN_EXE_td-mta")),
            binding: profile.binding_for(account).unwrap(),
            slots: &slots,
            clock: &clock,
            pending: &pending,
            password: device.password(),
        }
        .create(
            uploads::Input {
                account,
                device: device.device_id(),
                bytes: b"account upload",
                deadline: cap,
            },
            &mut meter,
        )
    };
    sql(fixture, "CREATE FUNCTION public.delay_first_upload() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(9); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_first_upload AFTER INSERT ON td_mta.upload_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.account=decode(repeat('aa',16),'hex')) EXECUTE FUNCTION public.delay_first_upload()");
    let [first, second] = accounts;
    assert_eq!(
        publish(first, 3_000),
        Err(uploads::Error::Publication(
            postgres_uploads::Error::Indeterminate
        ))
    );
    assert!(pending.operation().unwrap().is_some());
    publish(second, 10_000).unwrap();
    assert!(pending.operation().unwrap().is_none());
    // Wait for the already-issued COMMIT without reissuing either upload.
    sql(
        fixture,
        "DROP TRIGGER delay_first_upload ON td_mta.upload_receipts; DROP FUNCTION public.delay_first_upload()",
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(DISTINCT account), count(*) FROM td_mta.upload_receipts"
        ),
        "2|2"
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools required"]
fn real_postgres_multiple_account_sessions_are_isolated() {
    super::exercise_configuration(super::ConfigurationExercise::MultiAccount);
}

pub(super) fn submit_draft(
    account: AccountId,
    identity: IdentityId,
    email: &str,
    post: impl Fn(&str) -> String,
) -> String {
    let mut account_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(account, &mut account_bytes).unwrap();
    let mut identity_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let identity = td_mta::wire::encode_id(identity, &mut identity_bytes).unwrap();
    let mailboxes = post(
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
    let reply = post(&body);
    let value = td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
    value["methodResponses"][1][1]["created"]["submission"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"))
        .to_string()
}
