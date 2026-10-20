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

pub(super) fn exercise(
    fixture: &Fixture,
    smtp: TcpListener,
    https: TcpListener,
    relay: TcpListener,
    dns: std::net::UdpSocket,
) {
    let relay = super::serving_relay::Relay::new(fixture, relay, dns);
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
        let status = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["logging-status", "--runtime"])
            .arg(&runtime)
            .output()
            .unwrap();
        assert!(status.status.success());
        let status = td_json::parse(std::str::from_utf8(&status.stdout).unwrap()).unwrap();
        assert_eq!(status["status"].as_str(), Some("ok"));
        assert_eq!(status["write_failures"].as_u64(), Some(0));
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
    assert!(reply.contains("urn:ietf:params:jmap:submission"), "{reply}");
    let post = |body: &str| {
        let (mut tls, _) = super::smtp_transport::upgrade_protocol(
            TcpStream::connect(https_address).unwrap(),
            Arc::new(td_mta::clock::RuntimeClock::new()),
            &fixture.ca,
            td_crypto::TlsProtocol::Http1,
        );
        write!(tls,"POST /jmap/api HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",https_address.port(),encoded,body.len(),body).unwrap();
        tls.flush().unwrap();
        let mut reply = String::new();
        tls.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap()
    };
    let encode = |value: &str| {
        let id = td_mta::ids::MailboxId::parse(value).unwrap();
        let mut bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        td_mta::wire::encode_id(id, &mut bytes).unwrap().to_owned()
    };
    let mailbox = encode(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes ORDER BY id LIMIT 1",
    ));
    let mut account_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account_wire = td_mta::wire::encode_id(account, &mut account_bytes).unwrap();
    let identity = td_mta::ids::IdentityId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let mut identity_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let identity = td_mta::wire::encode_id(identity, &mut identity_bytes).unwrap();
    // Exercise live upload, structured composition and later SMTP delivery.
    let attachment_blob = {
        let (mut tls, _) = super::smtp_transport::upgrade_protocol(
            TcpStream::connect(https_address).unwrap(),
            Arc::new(td_mta::clock::RuntimeClock::new()),
            &fixture.ca,
            td_crypto::TlsProtocol::Http1,
        );
        write!(tls, "POST /jmap/upload/{account_wire} HTTP/1.1\r\nHost: localhost:{}\r\nAuthorization: Basic {}\r\nContent-Type: application/octet-stream\r\nContent-Length: 3\r\nConnection: close\r\n\r\n", https_address.port(), encoded).unwrap();
        tls.write_all(&[0, 1, 255]).unwrap();
        tls.flush().unwrap();
        let mut reply = String::new();
        tls.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        td_json::parse(reply.split_once("\r\n\r\n").unwrap().1).unwrap()["blobId"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let enqueue = || {
        let body=td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[
            ["Email/set",{"accountId":account_wire,"create":{"draft":{
                "mailboxIds":{mailbox.as_str():true},"keywords":{"$draft":true,"$seen":true},"from":[{"email":"main@example.test"}],"to":[{"email":"recipient@example.test"}],"attachments":[{"blobId":attachment_blob.as_str(),"name":"bytes.bin","type":"application/octet-stream","disposition":"attachment"}],"subject":"Executable relay","messageId":["relay@example.test"],"sentAt":"2026-10-13T12:00:59Z","textBody":[{"partId":"text","type":"text/plain"}],"bodyValues":{"text":{"value":"Sent by the executable.","isEncodingProblem":false,"isTruncated":false}}
            }}},"0"],
            ["EmailSubmission/set",{"accountId":account_wire,"create":{"submission":{"emailId":"#draft","identityId":identity}}},"1"]
        ]}).to_string();
        let reply = post(&body);
        assert!(
            reply["methodResponses"][1][1]["created"]["submission"]["id"]
                .as_str()
                .is_some(),
            "{reply}"
        );
        td_mta::wire::Id::parse(
            reply["methodResponses"][1][1]["created"]["submission"]["id"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
        .local::<td_mta::ids::SubmissionId>()
        .unwrap()
    };
    let cancel = |submission: td_mta::ids::SubmissionId| {
        let mut bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let id = td_mta::wire::encode_id(submission, &mut bytes).unwrap();
        let reply = post(&td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],"methodCalls":[["EmailSubmission/set",{"accountId":account_wire,"update":{id:{"undoStatus":"canceled"}}},"cancel"]]}).to_string());
        (reply, id.to_owned())
    };
    let first = enqueue();
    let wait = |query: &str, expected: &str, seconds: u64| {
        let end = std::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            if sql(fixture, query) == expected {
                break;
            }
            assert!(std::time::Instant::now() < end, "timed out: {query}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait(
        "SELECT count(*) FROM td_mta.recipients WHERE state=3",
        "1",
        30,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&cloud_runtime)
        .output()
        .unwrap();
    assert!(output.status.success());
    super::wait_command(&mut cloud);
    enqueue();
    // Supply a valid immutable eight-bit transmitted source before dispatch.
    use td_crypto::{Crypto, Digest};
    let bytes="From: main@example.test\r\nTo: recipient@example.test\r\nSubject: Eight bit\r\nContent-Transfer-Encoding: 8bit\r\n\r\ncafé\r\n".as_bytes();
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(bytes).unwrap();
    let digest = hash
        .finish()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let data = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    sql(fixture,&format!("UPDATE td_mta.blob_chunks SET bytes=decode('{data}','hex') WHERE blob_id IN (SELECT transmitted_blob_id FROM td_mta.submissions s WHERE EXISTS(SELECT 1 FROM td_mta.recipients r WHERE r.submission_id=s.id AND state=1)); UPDATE td_mta.blobs SET length={},digest=decode('{digest}','hex') WHERE id IN (SELECT transmitted_blob_id FROM td_mta.submissions s WHERE EXISTS(SELECT 1 FROM td_mta.recipients r WHERE r.submission_id=s.id AND state=1))",bytes.len()));
    sql(fixture, "UPDATE td_mta.account_usage u SET body_bytes=(SELECT sum(length) FROM td_mta.blobs b WHERE b.account=u.account),queue_bytes=(SELECT sum(length) FROM td_mta.blobs b WHERE b.account=u.account AND EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id))");
    sql(fixture,"INSERT INTO td_mta.route_health SELECT account,route_name,destination,decode('0000000000000001','hex'),3,3,(extract(epoch FROM clock_timestamp())*1000)::bigint-600000,(extract(epoch FROM clock_timestamp())*1000)::bigint-1000,NULL FROM td_mta.route_owners");
    let canceled = enqueue();
    let (reply, id) = cancel(canceled);
    assert!(
        reply["methodResponses"][0][1]["updated"]
            .get(id.as_str())
            .unwrap()
            .is_null(),
        "{reply}"
    );
    let (reply, id) = cancel(canceled);
    assert!(
        reply["methodResponses"][0][1]["updated"]
            .get(id.as_str())
            .unwrap()
            .is_null(),
        "{reply}"
    );
    let expired = enqueue();
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.submissions SET expires_at=send_at WHERE id=decode('{expired}','hex')"
        ),
    );
    let restarted = start("cloud");
    cloud = restarted.0;
    wait(
        "SELECT count(*) FROM td_mta.recipients WHERE state=4",
        "1",
        30,
    );
    wait("SELECT failure_count FROM td_mta.route_health", "0", 30);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE state=3"
        ),
        "1"
    );
    // Real monotonic startup guard and persisted five-minute minimum, no clock bypass.
    wait(
        "SELECT count(*) FROM td_mta.recipients WHERE state=4",
        "2",
        360,
    );
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 2);
    let clock_command = |operation: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args([operation, "--runtime"])
            .arg(&cloud_runtime)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    // A local publication failure must leave Pending without partial mail.
    sql(fixture, "CREATE SEQUENCE public.notice_attempts; CREATE FUNCTION public.refuse_notice() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog AS $$ BEGIN PERFORM nextval('public.notice_attempts'); RAISE EXCEPTION 'fixture storage refusal'; END $$; CREATE TRIGGER refuse_notice BEFORE INSERT ON td_mta.emails FOR EACH ROW WHEN (NEW.origin=4) EXECUTE FUNCTION public.refuse_notice()");
    assert!(clock_command("queue-clock-status").contains("held"));
    assert!(clock_command("queue-clock-confirm").contains("requested"));
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !clock_command("queue-clock-status").contains("confirmed") {
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(Duration::from_millis(50));
    }
    wait(
        "SELECT is_called::text FROM public.notice_attempts",
        "true",
        30,
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submissions WHERE notification=1"
        ),
        "1"
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.emails WHERE origin=4"),
        "0"
    );
    // Lose the reply after COMMIT has entered a deferred trigger. The notice
    // marker and local mail must still settle together, without replay duplicates.
    sql(fixture, "DROP TRIGGER refuse_notice ON td_mta.emails; DROP FUNCTION public.refuse_notice(); DROP SEQUENCE public.notice_attempts; CREATE FUNCTION public.arm_notice() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM set_config('statement_timeout','10000',true); RETURN NEW; END $$; CREATE TRIGGER arm_notice BEFORE UPDATE ON td_mta.submissions FOR EACH ROW WHEN (NEW.notification=2 AND OLD.notification=1) EXECUTE FUNCTION public.arm_notice(); CREATE FUNCTION public.delay_notice() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_notice AFTER UPDATE ON td_mta.submissions DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.notification=2 AND OLD.notification=1) EXECUTE FUNCTION public.delay_notice()");
    wait(
        "SELECT count(*) FROM td_mta.submissions WHERE notification=2",
        "1",
        30,
    );
    sql(
        fixture,
        "DROP TRIGGER delay_notice ON td_mta.submissions; DROP FUNCTION public.delay_notice(); DROP TRIGGER arm_notice ON td_mta.submissions; DROP FUNCTION public.arm_notice()",
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.emails WHERE origin=4"),
        "1"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.memberships m JOIN td_mta.emails e ON e.account=m.account AND e.id=m.email_id JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE e.origin=4 AND b.role='inbox' AND NOT EXISTS(SELECT 1 FROM td_mta.keywords k WHERE k.account=e.account AND k.email_id=e.id AND k.keyword='$seen')"), "1");
    let notice_wire = |submission: td_mta::ids::SubmissionId| {
        let id = td_mta::ids::EmailId::parse(&sql(fixture, &format!("SELECT encode(notification_email_id,'hex') FROM td_mta.submissions WHERE id=decode('{submission}','hex')"))).unwrap();
        let mut bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        td_mta::wire::encode_id(id, &mut bytes).unwrap().to_owned()
    };
    let notice_text = |id: &str| {
        let reply=post(&td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/get",{"accountId":account_wire,"ids":[id],"properties":["id","subject","textBody","bodyValues"],"fetchTextBodyValues":true},"notice"]]}).to_string());
        let email = &reply["methodResponses"][0][1]["list"][0];
        let part = email["textBody"][0]["partId"].as_str().unwrap();
        email["bodyValues"][part]["value"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let expiry_notice = notice_wire(expired);
    assert!(notice_text(&expiry_notice).contains("Delivery deadline expired"));
    assert!(cloud.0.try_wait().unwrap().is_none());
    assert!(clock_command("queue-clock-status").contains("confirmed"));
    // Recreate a worker after its uncertain publication; Stored is its receipt.
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&cloud_runtime)
        .output()
        .unwrap();
    assert!(output.status.success());
    super::wait_command(&mut cloud);
    cloud = start("cloud").0;
    enqueue();
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !relay.interrupted.load(std::sync::atomic::Ordering::Acquire) {
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    sql(fixture,"UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint WHERE id IN (SELECT submission_id FROM td_mta.submission_attempts WHERE active)");
    cloud.0.kill().unwrap();
    cloud.0.wait().unwrap();
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "1"
    );
    cloud = start("cloud").0;
    wait(
        "SELECT count(*) FROM td_mta.submission_attempts WHERE active",
        "0",
        60,
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE state=3 AND uncertain=0"
        ),
        "1"
    );
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.recipients r JOIN td_mta.submissions s ON s.id=r.submission_id WHERE r.state=3 AND r.next_attempt_at=s.expires_at"),"1");
    // A repaired configuration alone leaves the TLS latch; the named local
    // operation must explicitly release it under the new selected generation.
    let request = fixture.request();
    let (selected, candidate) =
        td_mta::postgres_selection::read(&request, &trust, deadline(), epoch)
            .unwrap()
            .unwrap();
    let generation = selected
        .generation
        .to_be_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    sql(fixture,&format!("UPDATE td_mta.route_health SET failure_count=1,reason=4,failed_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-600000,next_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1000,repair_generation=decode('{generation}','hex')"));
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&cloud_runtime)
        .output()
        .unwrap();
    assert!(output.status.success());
    super::wait_command(&mut cloud);
    let version = td_mta::postgres_configuration_store::Version::from_bytes([240; 16]);
    td_mta::postgres_configuration_store::publish(
        &request,
        &trust,
        deadline(),
        version,
        &candidate,
    )
    .unwrap();
    td_mta::postgres_selection::publish(
        &request,
        &trust,
        deadline(),
        td_mta::postgres_selection::prepare(epoch, selected.generation, version, &candidate)
            .unwrap(),
    )
    .unwrap();
    cloud = start("cloud").0;
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.route_health WHERE repair_generation IS NOT NULL"
        ),
        "1"
    );
    assert!(clock_command("queue-route-repair").contains("requested"));
    let end = std::time::Instant::now() + Duration::from_secs(15);
    while !clock_command("queue-route-status").contains("repaired") {
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.route_health WHERE repair_generation IS NOT NULL"
        ),
        "0"
    );
    // The frontend also reloads the new selected configuration on restart.
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&desktop_runtime)
        .output()
        .unwrap();
    assert!(output.status.success());
    super::wait_command(&mut desktop);
    desktop = start("desktop").0;
    enqueue();
    wait(
        "SELECT count(*) FROM td_mta.recipients WHERE state=4",
        "3",
        30,
    );
    let (reply, id) = cancel(first);
    assert_eq!(
        reply["methodResponses"][0][1]["notUpdated"][id.as_str()]["type"].as_str(),
        Some("cannotUnsend")
    );
    let refused = enqueue();
    wait(
        "SELECT count(*) FROM td_mta.submissions WHERE notification=2",
        "2",
        30,
    );
    // Cancel while the separate relay process is paused before MAIL's reply.
    let active_cancel = enqueue();
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !relay
        .cancel_waiting
        .load(std::sync::atomic::Ordering::Acquire)
    {
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    let (reply, id) = cancel(active_cancel);
    assert!(
        reply["methodResponses"][0][1]["updated"]
            .get(id.as_str())
            .unwrap()
            .is_null(),
        "{reply}"
    );
    relay
        .cancel_release
        .store(true, std::sync::atomic::Ordering::Release);
    wait(
        "SELECT count(*) FROM td_mta.recipients WHERE state=6",
        "2",
        30,
    );
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !relay
        .cancel_closed
        .load(std::sync::atomic::Ordering::Acquire)
    {
        assert!(std::time::Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(relay.accepted.load(std::sync::atomic::Ordering::Acquire), 3);
    let refused_notice = notice_wire(refused);
    let text = notice_text(&refused_notice);
    assert!(text.contains("Permanent SMTP refusal"), "{text}");
    assert!(text.contains("550 permanent fixture refusal"), "{text}");
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.emails WHERE origin=4"),
        "2"
    );
    let reply=post(&td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{"accountId":account_wire,"destroy":[expiry_notice.as_str()]},"delete-notice"]]}).to_string());
    assert_eq!(
        reply["methodResponses"][0][1]["destroyed"][0].as_str(),
        Some(expiry_notice.as_str())
    );
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["stop", "--runtime"])
        .arg(&cloud_runtime)
        .output()
        .unwrap();
    assert!(output.status.success());
    super::wait_command(&mut cloud);
    cloud = start("cloud").0;
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.emails WHERE origin=4"),
        "1"
    );
    assert_eq!(notice_wire(expired), expiry_notice);
    let (reply, id) = cancel(canceled);
    assert!(
        reply["methodResponses"][0][1]["updated"]
            .get(id.as_str())
            .unwrap()
            .is_null(),
        "{reply}"
    );
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.submission_attempts WHERE submission_id=decode('{canceled}','hex')")), "0");
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.submission_attempts WHERE submission_id=decode('{active_cancel}','hex') AND active")), "0");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submissions WHERE notification=2"
        ),
        "2"
    );
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
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "10");
    let utc_ms = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    td_mta::postgres_account_audit::inspect(
        &fixture.request(),
        &trust,
        deadline(),
        epoch,
        account,
        utc_ms,
        td_mta::postgres_account_audit::Limits {
            metadata: td_mta::metadata_sweep::Limits {
                rows: 10000,
                parent_reads: 10000,
            },
            blobs: 100,
            bytes: 1024 * 1024,
        },
    )
    .unwrap();
    assert!(sql(fixture,"SELECT count(*) FROM td_mta.application_logs WHERE event::json->'facts'->>'code'='metadata_committed'").parse::<usize>().unwrap()>0);
    assert!(sql(fixture,"SELECT count(*) FROM td_mta.application_logs WHERE event::json->'facts'->>'code'='service_stopped'").parse::<usize>().unwrap()>0);
    let output = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["postgres", "logs", "--connection-file"])
        .arg(&connection)
        .args(["--epoch", &epoch.to_string(), "--limit", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let page = td_json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 1);
    assert_eq!(page["retention_days"].as_u64(), Some(30));
}
