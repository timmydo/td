//! Real td-mail over the fixed HTTPS workers and the prepared PostgreSQL backend.
use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};
use td_json::{json, Json};
use td_mta::{
    admission::{timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits},
    clock::RuntimeClock,
    config::{endpoint::Origin, listener::Kind, materialize::ResolvedProfile},
    generations::{GenerationLease, GenerationSet},
    ports::{Clock, Deadline},
    postgres_runtime_profile::{self, Binding},
    smtp_receiving::Control,
    store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
    tls_policy::TlsPolicies,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Client {
    process: Process,
    input: std::process::ChildStdin,
    output: mpsc::Receiver<String>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Client {
    fn start(config: &std::path::Path, root: &std::path::Path) -> Self {
        Self::start_with_state(config, root, root)
    }
    fn start_with_state(
        config: &std::path::Path,
        root: &std::path::Path,
        state: &std::path::Path,
    ) -> Self {
        fs::create_dir_all(state).unwrap();
        let mut process = Process(
            Command::new(std::env::var_os("TD_MTA_TEST_MAIL").unwrap())
                .arg("--cli")
                .arg(format!("--config={}", config.display()))
                // Keep Unix socket names below sun_path even in long worktrees.
                .current_dir(root)
                .env("XDG_RUNTIME_DIR", ".")
                .env("HOME", state)
                .env("XDG_CONFIG_HOME", state.join("config"))
                .env("XDG_STATE_HOME", state.join("state"))
                .env("XDG_CACHE_HOME", state.join("cache"))
                .env("XDG_DATA_HOME", state.join("data"))
                .env("XDG_DOWNLOAD_DIR", state.join("downloads"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let input = process.0.stdin.take().unwrap();
        let output = process.0.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            process,
            input,
            output: receive,
            reader: Some(reader),
        }
    }
    fn ask(&mut self, request: Json) -> Json {
        writeln!(self.input, "{request}").unwrap();
        self.input.flush().unwrap();
        let line = self.output.recv_timeout(Duration::from_secs(30)).unwrap();
        td_json::parse(&line).unwrap()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.process.0.kill();
        let _ = self.process.0.wait();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}
struct Stop<'a>(&'a Control);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

pub(super) enum Mode {
    Read(TcpListener),
    Send(TcpListener),
}

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    configuration: &ResolvedProfile,
    policies: &GenerationLease<TlsPolicies>,
    mode: Mode,
) {
    let (socket, sending) = match mode {
        Mode::Read(socket) => (socket, false),
        Mode::Send(socket) => (socket, true),
    };
    use td_mta::{
        format::row::{ReceiptRecipients, ReceiptTls, SmtpReceipt},
        postgres_devices, postgres_receiving,
    };
    let device =
        postgres_devices::prepare_creation(binding.epoch(), binding.account(), "real client", 1)
            .unwrap();
    postgres_devices::publish_creation(request, trust, deadline(), &device).unwrap();
    let domain = GenerationSet::<TlsPolicies>::at_startup();
    let cloud = postgres_runtime_profile::load(
        request,
        trust,
        deadline(),
        binding.epoch(),
        "cloud",
        domain.reserve().unwrap(),
    )
    .unwrap();
    let message = concat!(
        "From: Sender <sender@example.test>\r\nTo: main@example.test\r\n",
        "Subject: PostgreSQL client\r\nMessage-ID: <postgres-client@example.test>\r\n",
        "Reply-To: Reply Desk <reply@example.test>\r\nCc: Peer <peer@example.test>\r\n",
        "References: <ancestor@example.test>\r\nDate: Tue, 14 Nov 2023 12:00:00 +0000\r\n",
        "X-Td-Fixture: =?utf-8?Q?caf=C3=A9_triage?=\r\n",
        "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n",
        "--x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello PostgreSQL client\r\n",
        "--x\r\nContent-Type: application/octet-stream\r\n",
        "Content-Disposition: attachment; filename=bytes.bin\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\nAAH/\r\n--x--\r\n"
    );
    let mut recipient_bytes = [0; 256];
    let recipients =
        ReceiptRecipients::encode(&["main@example.test"], &mut recipient_bytes).unwrap();
    let delivery = postgres_receiving::prepare(
        cloud.binding(),
        &postgres_receiving::Message {
            bytes: message.as_bytes(),
            hostname: "localhost",
            extended: true,
            header_bytes: 65536,
            received_at: 1_700_000_000_000,
            receipt: SmtpReceipt {
                peer: "192.0.2.1".parse().unwrap(),
                tls: ReceiptTls::Tls13,
                gateway: None,
                ehlo: "sender.example.test",
                reverse_path: "sender@example.test",
                recipients,
            },
        },
        deadline(),
    )
    .unwrap();
    postgres_receiving::publish(request, trust, deadline(), &delivery).unwrap();
    let cloud_binding = cloud.binding().clone();
    drop(cloud);
    let runtime = fixture.root.join("real-client");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let response_root = runtime.join("responses");
    fs::create_dir(&response_root).unwrap();
    fs::set_permissions(&response_root, fs::Permissions::from_mode(0o700)).unwrap();
    let mut root = PrivateRoot::open(response_root.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let resources = configuration.candidate().resources().resources();
    let admission = DiskLimits::default()
        .plan(resources, WorkLimits::default(), ViewMode::OnlineBackground)
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(RuntimeClock::new());
    let until = Deadline::after(clock.sample().unwrap().monotonic, 600_000).unwrap();
    let spool = RequestSpool::open(&mut root, clock.clone(), until).unwrap();
    let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
    let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
    let responses =
        ResponseCoordinator::new(spool, &admission, &mut states, &mut cells, clock.clone())
            .unwrap();
    let origin = format!("https://localhost:{}", socket.local_addr().unwrap().port());
    let make_session = if sending {
        td_mta::jmap_session::Session::mail_submission
    } else {
        td_mta::jmap_session::Session::mail_access
    };
    let session = make_session(
        Origin::parse(&origin).unwrap(),
        configuration
            .candidate()
            .identities()
            .unwrap()
            .account()
            .unwrap(),
        resources,
        td_mta::jmap_session::Concurrency {
            uploads: 1,
            requests: resources.https_body_workers(),
        },
    )
    .unwrap();
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let slots = td_mta::postgres_read::ReadSlots::new(2).unwrap();
    let authentication_accounts = [("user", binding)];
    let authentication = td_mta::postgres_http_auth::Authentication::new(
        &loaded,
        executable,
        &authentication_accounts,
        &clock,
    )
    .unwrap();
    let pending = td_mta::postgres_http_updates::Pending::default();
    let backend = td_mta::postgres_http_api::Service {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
        pending: &pending,
        responses: &responses,
        resources,
        session: &session,
        response_maximum: admission.disk().response_bytes,
        clock: &clock,
        admission: &admission,
        timeouts: &timeouts,
    };
    if !sending {
        exercise_rule_projection(
            &backend,
            device.device_id(),
            device.password(),
            delivery.email(),
        );
        assert_eq!(responses.used().unwrap(), 0);
        assert!(!responses.admission_stopped());
    }
    let config = runtime.join("mail.toml");
    fs::write(&config, format!("[account.test]\nwell_known_url = \"{origin}/.well-known/jmap\"\nusername = \"user\"\npassword_command = \"printf {}\"\n", std::str::from_utf8(device.password()).unwrap())).unwrap();
    let mut mail_config = fs::read_to_string(&config).unwrap();
    mail_config.push_str("[mail]\nreply_from = \"main@example.test\"\n");
    fs::write(&config, mail_config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let fetch_socket = runtime.join("td-fetch/socket");
    let _fetch = Process(
        Command::new(std::env::var_os("TD_MTA_TEST_FETCH").unwrap())
            .args([
                "--exact",
                "fetchd::tests::mail_read_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("TD_TEST_FETCH_ROOT", fixture.root.join("root.der"))
            .current_dir(&runtime)
            .env("TD_TEST_FETCH_SOCKET", "td-fetch/socket")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let stop_at = Instant::now() + Duration::from_secs(10);
    while !fetch_socket.exists() {
        assert!(Instant::now() < stop_at, "fetch fixture startup");
        thread::sleep(Duration::from_millis(10));
    }
    let handshakes = td_mta::tls_admission::HandshakePool::new(2).unwrap();
    let ready = AtomicBool::new(false);
    let control = Control::default();
    configuration.candidate().with_graph(|records, text| {
        let listeners = records.view(text).unwrap().listeners().unwrap();
        let row = (0..listeners.len()).filter_map(|i| listeners.listener(i).unwrap())
            .find(|row| row.kind == Kind::Https).unwrap();
        let listeners = [td_mta::https::BoundListener::new(socket, row, policies).unwrap()];
        let server = td_mta::https::Https {
            listeners: &listeners, policies, handshakes: &handshakes,
            authentication: &authentication, origins: &[Origin::parse(&origin).unwrap()],
            resources, timeouts: &timeouts, clock: clock.clone(),
            backends: &[&backend], ready: &ready,
        };
        thread::scope(|scope| {
            let worker = scope.spawn(|| server.run(&control));
            let stop = Stop(&control);
            while !ready.load(Ordering::Acquire) {
                assert!(Instant::now() < stop_at, "HTTPS fixture startup");
                thread::sleep(Duration::from_millis(1));
            }
            let mut client = Client::start(&config, &runtime);
            let connected = client.ask(json!({"command":"connect", "account":"test"}));
            assert!(connected["ok"].is_true(), "{connected}");
            if sending {
                exercise_send(&mut client, fixture, binding, &runtime, device.device_id(), device.password());
                drop(client);
                drop(stop);
                worker.join().unwrap().unwrap();
                return;
            }
            let mailboxes = client.ask(json!({"command":"list_mailboxes"}));
            assert!(mailboxes["ok"].is_true(), "{mailboxes}");
            let folders = mailboxes["mailboxes"].as_array().unwrap();
            assert_eq!(folders.len(), 1, "{mailboxes}");
            let listing = client.ask(json!({"command":"query_emails", "mailbox_id":folders[0]["id"], "limit":50}));
            assert!(listing["ok"].is_true(), "{listing}");
            let emails = listing["emails"].as_array().unwrap();
            assert_eq!(emails.len(), 1, "{listing}");
            let email = &emails[0];
            assert_eq!(email["subject"].as_str(), Some("PostgreSQL client"));
            assert_eq!(email["body"].as_str(), Some("Hello PostgreSQL client"));
            let thread = client.ask(json!({"command":"get_thread", "thread_id":email["thread_id"]}));
            assert!(thread["ok"].is_true(), "{thread}");
            let members = thread["emails"].as_array().unwrap();
            assert_eq!(members.len(), 1, "{thread}");
            assert_eq!(members[0]["id"], email["id"]);
            assert_eq!(members[0]["body"], email["body"]);
            exercise_reply_context(&mut client, email["id"].as_str().unwrap());
            for (command, seen, flagged) in [("mark_read", true, false), ("flag", true, true), ("mark_unread", false, true), ("unflag", false, false)] {
                let changed = client.ask(json!({"command":command, "id":email["id"]}));
                assert!(changed["ok"].is_true(), "{changed}");
                assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords WHERE keyword='$seen'"), if seen { "1" } else { "0" });
                assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords WHERE keyword='$flagged'"), if flagged { "1" } else { "0" });
                let fresh = client.ask(json!({"command":"query_emails", "mailbox_id":folders[0]["id"], "limit":50}));
                assert!(fresh["ok"].is_true(), "{fresh}");
                assert_eq!(fresh["emails"][0]["is_read"].as_bool(), Some(seen), "{fresh}");
                assert_eq!(fresh["emails"][0]["is_flagged"].as_bool(), Some(flagged), "{fresh}");
            }
            let created = client.ask(json!({"command":"create_mailbox", "name":"Archive"}));
            assert!(created["ok"].is_true(), "{created}");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes WHERE name='Archive'"), "1");
            let duplicate = client.ask(json!({"command":"create_mailbox", "name":"Archive"}));
            assert!(!duplicate["ok"].is_true(), "{duplicate}");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes WHERE name='Archive'"), "1");
            let listing = client.ask(json!({"command":"list_mailboxes"}));
            assert!(listing["ok"].is_true(), "{listing}");
            let archive_wire = listing["mailboxes"].as_array().unwrap().iter().find(|folder| folder["name"].as_str() == Some("Archive")).unwrap()["id"].as_str().unwrap();
            let archive = sql(fixture, "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE name='Archive'");
            let inbox_wire = folders[0]["id"].as_str().unwrap();
            let inbox = sql(fixture, "SELECT encode(mailbox_id,'hex') FROM td_mta.memberships");
            for (destination, expected) in [(archive_wire, archive), (inbox_wire, inbox)] {
                let moved = client.ask(json!({"command":"move_email", "id":email["id"], "to_mailbox_id":destination}));
                assert!(moved["ok"].is_true(), "{moved}");
                assert_eq!(sql(fixture, "SELECT encode(mailbox_id,'hex') FROM td_mta.memberships"), expected);
                assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords WHERE keyword IN ('$seen','$flagged')"), "0");
                let listing = client.ask(json!({"command":"query_emails", "mailbox_id":destination, "limit":50}));
                assert!(listing["ok"].is_true(), "{listing}");
                assert_eq!(listing["emails"].as_array().unwrap().len(), 1, "{listing}");
                assert_eq!(listing["emails"][0]["id"], email["id"]);
                assert_eq!(listing["emails"][0]["blob_id"], email["blob_id"]);
                if destination == archive_wire {
                    let refused = client.ask(json!({"command":"delete_mailbox", "mailbox_id":archive_wire}));
                    assert!(!refused["ok"].is_true(), "nonempty folder: {refused}");
                    assert_eq!(sql(fixture, "SELECT encode(mailbox_id,'hex') FROM td_mta.memberships"), expected);
                }
            }
            let before = sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts");
            let refused = client.ask(json!({"command":"delete_mailbox", "mailbox_id":inbox_wire}));
            assert!(!refused["ok"].is_true(), "protected Inbox: {refused}");
            assert_eq!(sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts"), before);
            let created = client.ask(json!({"command":"create_mailbox", "name":"Child"}));
            assert!(created["ok"].is_true(), "{created}");
            sql(fixture, "UPDATE td_mta.mailboxes SET parent_id=(SELECT id FROM td_mta.mailboxes WHERE name='Archive') WHERE name='Child'");
            let before = sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts");
            let refused = client.ask(json!({"command":"delete_mailbox", "mailbox_id":archive_wire}));
            assert!(!refused["ok"].is_true(), "parent folder: {refused}");
            assert_eq!(sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts"), before);
            sql(fixture, "UPDATE td_mta.mailboxes SET parent_id=NULL WHERE name='Child'");
            let deleted = client.ask(json!({"command":"delete_mailbox", "mailbox_id":archive_wire}));
            assert!(deleted["ok"].is_true(), "{deleted}");
            let after = sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts");
            assert_eq!(u64::from_str_radix(&after,16).unwrap(), u64::from_str_radix(&before,16).unwrap()+1);
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes WHERE name='Archive'"), "0");
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.changes WHERE kind=1 AND action=3"), "1");
            let again = client.ask(json!({"command":"delete_mailbox", "mailbox_id":archive_wire}));
            assert!(!again["ok"].is_true(), "missing folder: {again}");
            assert_eq!(sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts"), after);
            let missing = client.ask(json!({"command":"delete_mailbox", "mailbox_id":"foreign-folder"}));
            assert!(!missing["ok"].is_true(), "foreign ID: {missing}");
            let listing = client.ask(json!({"command":"list_mailboxes"}));
            assert!(listing["ok"].is_true(), "{listing}");
            assert!(!listing["mailboxes"].as_array().unwrap().iter().any(|folder| folder["id"].as_str()==Some(archive_wire)));
            exercise_queries(&mut client, request, trust, &cloud_binding, inbox_wire,
                listing["mailboxes"].as_array().unwrap().iter().find(|folder| folder["name"].as_str()==Some("Child")).unwrap()["id"].as_str().unwrap(), email["id"].as_str().unwrap());
            exercise_header_rules(fixture, &config, &runtime, inbox_wire, email["id"].as_str().unwrap());
            let usage_before = sql(fixture, "SELECT body_bytes||':'||blob_count FROM td_mta.account_usage");
            let reply_message = message
                .replace("Message-ID: <postgres-client@example.test>", "Message-ID: <postgres-reply@example.test>")
                .replace("References: <ancestor@example.test>", "References: <ancestor@example.test> <postgres-client@example.test>")
                .replace("Subject: PostgreSQL client", "Subject: Re: PostgreSQL client")
                .replace("Hello PostgreSQL client", "Reply through PostgreSQL");
            let extra = postgres_receiving::prepare(&cloud_binding, &postgres_receiving::Message {
                bytes: reply_message.as_bytes(), hostname: "localhost", extended: true, header_bytes: 65536,
                received_at: 1_700_000_001_000,
                receipt: SmtpReceipt { peer: "192.0.2.1".parse().unwrap(), tls: ReceiptTls::Tls13, gateway: None,
                    ehlo: "sender.example.test", reverse_path: "sender@example.test", recipients },
            }, deadline()).unwrap();
            postgres_receiving::publish(request, trust, deadline(), &extra).unwrap();
            let mut extra_wire = [0; td_mta::wire::IdKind::WIRE_BYTES];
            let extra_wire = td_mta::wire::encode_id(extra.email(), &mut extra_wire).unwrap();
            let listing = client.ask(json!({"command":"query_emails", "mailbox_id":inbox_wire, "limit":50}));
            assert_eq!(listing["emails"].as_array().unwrap().len(), 2, "{listing}");
            let check_thread = |client: &mut Client, expected: &[(&str, &str)]| {
                let reply = client.ask(json!({"command":"get_thread", "thread_id":email["thread_id"]}));
                assert!(reply["ok"].is_true(), "{reply}");
                let members = reply["emails"].as_array().unwrap();
                let actual: Vec<_> = members.iter().map(|member| {
                    assert_eq!(member["thread_id"], email["thread_id"], "{reply}");
                    (member["id"].as_str().unwrap(), member["body"].as_str().unwrap())
                }).collect();
                assert_eq!(actual, expected, "{reply}");
            };
            let original = (email["id"].as_str().unwrap(), "Hello PostgreSQL client");
            let reply = (extra_wire, "Reply through PostgreSQL");
            check_thread(&mut client, &[original, reply]);
            let thread_folders = client.ask(json!({"command":"list_mailboxes"}));
            assert!(thread_folders["ok"].is_true(), "{thread_folders}");
            let other = thread_folders["mailboxes"].as_array().unwrap().iter()
                .find(|folder| folder["name"].as_str() == Some("Child")).unwrap()["id"].as_str().unwrap();
            let moved = client.ask(json!({"command":"move_email", "id":extra_wire, "to_mailbox_id":other}));
            assert!(moved["ok"].is_true(), "{moved}");
            check_thread(&mut client, &[original, reply]);
            let inbox = client.ask(json!({"command":"query_emails", "mailbox_id":inbox_wire}));
            assert!(inbox["ok"].is_true(), "{inbox}");
            assert_eq!(inbox["emails"].as_array().unwrap().len(), 1, "{inbox}");
            assert_eq!(inbox["emails"][0]["id"], email["id"]);
            let deleted = client.ask(json!({"command":"destroy", "id":extra_wire}));
            assert!(deleted["results"][0]["ok"].is_true(), "{deleted}");
            check_thread(&mut client, &[original]);
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
            assert_eq!(sql(fixture, "SELECT body_bytes||':'||blob_count FROM td_mta.account_usage"), usage_before);
            let listing = client.ask(json!({"command":"query_emails", "mailbox_id":inbox_wire, "limit":50}));
            assert_eq!(listing["emails"].as_array().unwrap().len(), 1, "{listing}");
            assert_eq!(listing["emails"][0]["id"], email["id"]);
            assert_eq!(pending.operation().unwrap(), None);
            sql(fixture, "CREATE FUNCTION public.refuse_client_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture commit uncertainty'; END $$; CREATE CONSTRAINT TRIGGER refuse_client_commit AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.refuse_client_commit()");
            let uncertain = client.ask(json!({"command":"flag", "id":email["id"]}));
            assert!(uncertain["ok"].is_true(), "queued intention: {uncertain}");
            let operation = pending.operation().unwrap().unwrap();
            let blocked = client.ask(json!({"command":"flag", "id":email["id"]}));
            assert!(blocked["ok"].is_true(), "queued intention: {blocked}");
            assert_eq!(pending.operation().unwrap(), Some(operation));
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords WHERE keyword='$flagged'"), "0");
            drop(client);
            let mut client = Client::start_with_state(&config, &runtime, &runtime.join("fresh-reader"));
            let connected = client.ask(json!({"command":"connect", "account":"test"}));
            assert!(connected["ok"].is_true(), "{connected}");
            let settlement_slots = td_mta::postgres_read::ReadSlots::new(1).unwrap();
            assert!(pending.settle(&loaded, executable, &settlement_slots, &clock,
                Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap()).unwrap().is_none());
            assert_eq!(pending.operation().unwrap(), Some(operation));

            let fresh = client.ask(json!({"command":"query_emails", "mailbox_id":folders[0]["id"], "limit":50}));
            assert!(fresh["ok"].is_true(), "{fresh}");
            assert!(!fresh["emails"][0]["is_flagged"].is_true());
            sql(fixture, "DROP TRIGGER refuse_client_commit ON td_mta.flag_receipts; DROP FUNCTION public.refuse_client_commit()");

            let attachment = &email["attachments"][0];
            let download = client.ask(json!({"command":"download_attachment", "blob_id":attachment["blob_id"], "name":"bytes.bin"}));
            assert!(download["ok"].is_true(), "{download}");
            assert_eq!(fs::read(download["path"].as_str().unwrap()).unwrap(), [0, 1, 255]);
            let raw = client.ask(json!({"command":"download_attachment", "blob_id":email["blob_id"], "name":"raw.eml", "content_type":"message/rfc822"}));
            assert!(raw["ok"].is_true(), "{raw}");
            assert!(fs::read(raw["path"].as_str().unwrap()).unwrap().ends_with(message.as_bytes()));
            drop(client);
            drop(stop);
            worker.join().unwrap().unwrap();
        });
    }).unwrap();
    if sending {
        assert_eq!(pending.submissions.operation().unwrap(), None);
        assert_eq!(pending.drafts.operation().unwrap(), None);
        assert_eq!(pending.operation().unwrap(), None);
        assert_eq!(responses.used().unwrap(), 0);
        return;
    }
    // A separate owner exercises a lost acknowledgement whose receipt commits.
    // The HTTP owner's absent receipt above remains fenced.
    let confirmed = td_mta::postgres_http_updates::Pending::default();
    let updates = td_mta::postgres_http_updates::Updates {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
        clock: &clock,
        pending: &confirmed,
        password: device.password(),
    };
    let email_hex = sql(fixture, "SELECT encode(id,'hex') FROM td_mta.emails");
    let email = td_mta::ids::EmailId::parse(&email_hex).unwrap();
    sql(fixture, "CREATE FUNCTION public.delay_client_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_client_commit AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_client_commit()");
    let until = Deadline::after(clock.sample().unwrap().monotonic, 2_000).unwrap();
    let mut meter = td_mta::admission::work::Meter::new(
        until,
        td_mta::admission::work::Charge {
            records: 10000,
            ..Default::default()
        },
    );
    let completion = td_mta::jmap_request::dispatch::EmailMutator::update(
        &updates,
        td_mta::store_fs::EmailUpdateRequest {
            account: binding.account(),
            device: device.device_id(),
            expected: None,
            updates: &[td_mta::store_fs::EmailUpdate {
                email,
                mailbox: None,
                seen: None,
                flagged: Some(true),
            }],
            deadline: until,
        },
        &mut meter,
    )
    .unwrap();
    assert!(matches!(
        completion.outcome,
        Err(td_mta::ports::CommitFailure::Indeterminate(_))
    ));
    let operation = confirmed.operation().unwrap().unwrap();
    thread::sleep(Duration::from_secs(5));
    sql(fixture, "DROP TRIGGER delay_client_commit ON td_mta.flag_receipts; DROP FUNCTION public.delay_client_commit()");
    let first = td_mta::postgres_read::Snapshot::open_resolved(
        &slots,
        &loaded,
        executable,
        deadline(),
        binding,
        device.password(),
    )
    .unwrap();
    let second = td_mta::postgres_read::Snapshot::open_resolved(
        &slots,
        &loaded,
        executable,
        deadline(),
        binding,
        device.password(),
    )
    .unwrap();
    let settlement_slots = td_mta::postgres_read::ReadSlots::new(1).unwrap();
    let settled = confirmed
        .settle(
            &loaded,
            executable,
            &settlement_slots,
            &clock,
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(
        settled.outcome.results,
        [td_mta::postgres_email_set::UpdateResult::Updated]
    );
    assert!(settled.outcome.state.new_sequence > settled.outcome.state.old_sequence);
    assert_eq!(confirmed.operation().unwrap(), None);
    drop((first, second));
    sql(fixture, "CREATE FUNCTION public.delay_destroy_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_destroy_commit AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_destroy_commit()");
    let until = Deadline::after(clock.sample().unwrap().monotonic, 2_000).unwrap();
    let mut meter = td_mta::admission::work::Meter::new(
        until,
        td_mta::admission::work::Charge {
            records: 10000,
            ..Default::default()
        },
    );
    let completion = td_mta::jmap_request::dispatch::EmailMutator::destroy(
        &updates,
        td_mta::store_fs::EmailDestroyRequest {
            account: binding.account(),
            device: device.device_id(),
            expected: None,
            emails: &[email],
            deadline: until,
        },
        &mut meter,
    )
    .unwrap();
    assert!(matches!(
        completion.outcome,
        Err(td_mta::ports::CommitFailure::Indeterminate(_))
    ));
    let operation = confirmed.operation().unwrap().unwrap();
    thread::sleep(Duration::from_secs(5));
    sql(fixture, "DROP TRIGGER delay_destroy_commit ON td_mta.flag_receipts; DROP FUNCTION public.delay_destroy_commit()");
    let settled = confirmed
        .settle(
            &loaded,
            executable,
            &settlement_slots,
            &clock,
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(
        settled.outcome.results,
        [td_mta::postgres_email_set::UpdateResult::Destroyed]
    );
    assert_eq!(confirmed.operation().unwrap(), None);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    assert_eq!(
        sql(
            fixture,
            "SELECT body_bytes||':'||blob_count FROM td_mta.account_usage"
        ),
        "0:0"
    );

    assert_eq!(responses.used().unwrap(), 0);
    assert!(!responses.admission_stopped());
    let folders = td_mta::postgres_http_mailboxes::Mailboxes {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
        clock: &clock,
        pending: &confirmed.mailboxes,
        password: device.password(),
    };
    sql(fixture, "CREATE FUNCTION public.delay_folder_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_folder_commit AFTER INSERT ON td_mta.mailbox_create_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_folder_commit()");
    let until = Deadline::after(clock.sample().unwrap().monotonic, 2_000).unwrap();
    let mut meter = td_mta::admission::work::Meter::new(
        until,
        td_mta::admission::work::Charge {
            records: 10000,
            io_bytes: 65536,
            output_bytes: 65536,
            ..Default::default()
        },
    );
    let completion = td_mta::jmap_request::dispatch::MailboxMutator::create_mailboxes(
        &folders,
        td_mta::store_fs::MailboxCreateRequest {
            account: binding.account(),
            device: device.device_id(),
            expected: None,
            names: &["Recovered"],
            deadline: until,
        },
        &mut meter,
    )
    .unwrap();
    assert!(matches!(
        completion.outcome,
        Err(td_mta::ports::CommitFailure::Indeterminate(_))
    ));
    let operation = confirmed.mailboxes.operation().unwrap().unwrap();
    thread::sleep(Duration::from_secs(5));
    sql(fixture, "DROP TRIGGER delay_folder_commit ON td_mta.mailbox_create_receipts; DROP FUNCTION public.delay_folder_commit()");
    let settled = confirmed
        .mailboxes
        .settle(
            &loaded,
            executable,
            &settlement_slots,
            &clock,
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert!(matches!(
        settled.outcome.results.as_slice(),
        [td_mta::postgres_mailbox_set::ResultEntry::Created(_)]
    ));
    assert_eq!(confirmed.mailboxes.operation().unwrap(), None);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='Recovered'"
        ),
        "1"
    );
    let [td_mta::postgres_mailbox_set::ResultEntry::Created(recovered)] =
        settled.outcome.results.as_slice()
    else {
        panic!("created folder receipt")
    };
    let recovered = *recovered;
    sql(fixture, "CREATE FUNCTION public.delay_folder_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_folder_delete AFTER INSERT ON td_mta.mailbox_create_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_folder_delete()");
    let until = Deadline::after(clock.sample().unwrap().monotonic, 2_000).unwrap();
    let mut meter = td_mta::admission::work::Meter::new(
        until,
        td_mta::admission::work::Charge {
            records: 10000,
            io_bytes: 65536,
            output_bytes: 65536,
            ..Default::default()
        },
    );
    let completion = td_mta::jmap_request::dispatch::MailboxMutator::delete_mailbox(
        &folders,
        td_mta::store_fs::MailboxDeleteRequest {
            account: binding.account(),
            device: device.device_id(),
            expected: None,
            mailbox: Some(recovered),
            deadline: until,
        },
        &mut meter,
    )
    .unwrap();
    assert!(matches!(
        completion.outcome,
        Err(td_mta::ports::CommitFailure::Indeterminate(_))
    ));
    assert_eq!(completion.result, None);
    let operation = confirmed.mailboxes.operation().unwrap().unwrap();
    thread::sleep(Duration::from_secs(5));
    sql(fixture, "DROP TRIGGER delay_folder_delete ON td_mta.mailbox_create_receipts; DROP FUNCTION public.delay_folder_delete()");
    let settled = confirmed
        .mailboxes
        .settle(
            &loaded,
            executable,
            &settlement_slots,
            &clock,
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(
        settled.outcome.results,
        [td_mta::postgres_mailbox_set::ResultEntry::Destroyed(
            recovered
        )]
    );
    assert_eq!(confirmed.mailboxes.operation().unwrap(), None);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='Recovered'"
        ),
        "0"
    );
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.changes WHERE kind=1 AND action=3 AND object=decode('{recovered}','hex')")), "1");
}

fn exercise_rule_projection(
    backend: &td_mta::postgres_http_api::Service<'_, '_, '_>,
    device: td_mta::ids::DeviceId,
    password: &[u8],
    email: td_mta::ids::EmailId,
) {
    use td_mta::https_upload::Backend;
    let credentials = http_fixture::credentials(password);
    let mut buffer = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(backend.binding.account(), &mut buffer)
        .unwrap()
        .to_owned();
    let email = td_mta::wire::encode_id(email, &mut buffer)
        .unwrap()
        .to_owned();
    let absent =
        td_mta::wire::encode_id(td_mta::ids::EmailId::from_bytes([0xfe; 16]), &mut buffer).unwrap();
    // td-mail::get_emails_for_rules intentionally asks for no body properties.
    let properties = [
        "id",
        "threadId",
        "from",
        "to",
        "cc",
        "replyTo",
        "subject",
        "messageId",
        "receivedAt",
        "keywords",
        "mailboxIds",
        "header:X-Td-Fixture:asText",
        "header:X-Td-Missing:asText",
    ];
    let body = json!({"using":["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls":[["Email/get", {"accountId":account, "ids":[email, absent],
            "properties":properties}, "0"]]})
    .to_string();
    let mut wire = http_fixture::Wire {
        input: body.as_bytes().to_vec(),
        position: 0,
        output: Vec::new(),
        fail_write: false,
    };
    backend
        .api(
            td_mta::https_api::Request {
                device,
                credentials: &credentials,
                framing: td_mta::http_request::Framing::Length(body.len() as u64),
                prefix: &[],
                expect_continue: false,
                deadline: Deadline::after(backend.clock.sample().unwrap().monotonic, 10_000)
                    .unwrap(),
            },
            &mut wire,
            &Control::default(),
        )
        .unwrap();
    let wire = String::from_utf8(wire.output).unwrap();
    let (headers, body) = wire.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{wire}");
    let response = td_json::parse(body).unwrap();
    let methods = response["methodResponses"].as_array().unwrap();
    assert_eq!(methods.len(), 1, "{response}");
    assert_eq!(methods[0][0].as_str(), Some("Email/get"), "{response}");
    assert_eq!(methods[0][2].as_str(), Some("0"), "{response}");
    let result = &methods[0][1];
    assert_eq!(result["accountId"].as_str(), Some(account.as_str()));
    assert_eq!(result["notFound"], json!([absent]));
    let list = result["list"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{result}");
    let item = &list[0];
    assert_eq!(item["id"].as_str(), Some(email.as_str()));
    assert_eq!(item["subject"].as_str(), Some("PostgreSQL client"));
    assert_eq!(
        item["from"][0]["email"].as_str(),
        Some("sender@example.test")
    );
    assert_eq!(item["to"][0]["email"].as_str(), Some("main@example.test"));
    assert_eq!(item["cc"][0]["email"].as_str(), Some("peer@example.test"));
    assert_eq!(
        item["replyTo"][0]["email"].as_str(),
        Some("reply@example.test")
    );
    assert_eq!(item["messageId"], json!(["postgres-client@example.test"]));
    assert_eq!(
        item["header:X-Td-Fixture:asText"].as_str(),
        Some("café triage")
    );
    assert!(item
        .get("header:X-Td-Missing:asText")
        .is_some_and(Json::is_null));
    let object = item.as_object().unwrap();
    assert_eq!(object.len(), properties.len(), "{item}");
    for name in properties {
        assert!(item.get(name).is_some(), "missing {name}: {item}");
    }
    for name in [
        "bodyValues",
        "textBody",
        "htmlBody",
        "bodyStructure",
        "preview",
        "attachments",
        "blobId",
    ] {
        assert!(item.get(name).is_none(), "unrequested {name}: {item}");
    }
}

fn exercise_reply_context(client: &mut Client, id: &str) {
    for reply_all in [false, true] {
        let response = client.ask(json!({"command":"reply_draft", "id":id,
            "reply_all":reply_all}));
        assert!(response["ok"].is_true(), "{response}");
        let draft = response["draft"].as_str().unwrap();
        for expected in [
            "From: main@example.test\n",
            "To: Reply Desk <reply@example.test>\n",
            "Subject: Re: PostgreSQL client\n",
            "In-Reply-To: <postgres-client@example.test>\n",
            "References: <ancestor@example.test> <postgres-client@example.test>\n",
            "On 2023-11-14T12:00:00Z, Sender <sender@example.test> wrote:",
            "> Hello PostgreSQL client",
        ] {
            assert!(draft.contains(expected), "missing {expected:?}: {draft}");
        }
        let cc = draft.lines().find(|line| line.starts_with("Cc:"));
        assert_eq!(
            cc,
            reply_all.then_some("Cc: Peer <peer@example.test>"),
            "{draft}"
        );
    }
    let response = client.ask(json!({"command":"forward_draft", "id":id}));
    assert!(response["ok"].is_true(), "{response}");
    let draft = response["draft"].as_str().unwrap();
    for expected in [
        "From: main@example.test\nTo: \nSubject: Fwd: PostgreSQL client\n",
        "---------- Forwarded message ----------",
        "From: Sender <sender@example.test>\n",
        "Date: 2023-11-14T12:00:00Z\n",
        "Subject: PostgreSQL client\n",
        "To: main@example.test\n",
        "Cc: Peer <peer@example.test>\n",
        "Hello PostgreSQL client",
    ] {
        assert!(draft.contains(expected), "missing {expected:?}: {draft}");
    }
}

fn exercise_header_rules(
    fixture: &Fixture,
    config: &std::path::Path,
    runtime: &std::path::Path,
    mailbox: &str,
    id: &str,
) {
    let root = runtime.join("header-rules");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let rules_config = root.join("mail.toml");
    let mut contents = fs::read_to_string(config).unwrap();
    // The base configuration already ends in the mail table.
    contents.push_str("rules_mailbox_regex = \".*\"\n");
    fs::write(&rules_config, contents).unwrap();
    fs::set_permissions(&rules_config, fs::Permissions::from_mode(0o600)).unwrap();
    for (name, pattern, flagged) in [("miss", "^wrong$", false), ("match", "^café triage$", true)]
    {
        fs::write(root.join("rules.toml"), format!(
            "[[rule]]\nname = \"custom-header\"\n[rule.match]\nheader = \"X-Td-Fixture\"\nregex = \"{pattern}\"\n[rule.actions]\nflag = true\n"
        )).unwrap();
        // Fresh caches ensure an earlier processed marker cannot hide a rule.
        let mut client = Client::start_with_state(&rules_config, runtime, &root.join(name));
        let connected = client.ask(json!({"command":"connect", "account":"test"}));
        assert!(connected["ok"].is_true(), "{connected}");
        let mailboxes = client.ask(json!({"command":"list_mailboxes"}));
        assert!(mailboxes["ok"].is_true(), "{mailboxes}");
        let listing = client.ask(json!({"command":"query_emails", "mailbox_id":mailbox}));
        assert!(listing["ok"].is_true(), "{listing}");
        assert_eq!(listing["emails"].as_array().unwrap().len(), 1, "{listing}");
        assert_eq!(listing["emails"][0]["id"].as_str(), Some(id), "{listing}");
        assert_eq!(
            listing["emails"][0]["body"].as_str(),
            Some("Hello PostgreSQL client")
        );
        assert!(!listing["emails"][0]["unreadable"].is_true(), "{listing}");
        assert_eq!(
            sql(
                fixture,
                "SELECT count(*) FROM td_mta.keywords WHERE keyword='$flagged'"
            ),
            if flagged { "1" } else { "0" }
        );
        if flagged {
            let restored = client.ask(json!({"command":"unflag", "id":id}));
            assert!(restored["ok"].is_true(), "{restored}");
            assert_eq!(
                sql(
                    fixture,
                    "SELECT count(*) FROM td_mta.keywords WHERE keyword='$flagged'"
                ),
                "0"
            );
        }
    }
}

fn exercise_queries(
    client: &mut Client,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    inbox: &str,
    other: &str,
    original: &str,
) {
    use td_mta::{
        format::row::{ReceiptRecipients, ReceiptTls, SmtpReceipt},
        postgres_receiving,
    };
    let mut recipient_bytes = [0; 256];
    let recipients =
        ReceiptRecipients::encode(&["main@example.test"], &mut recipient_bytes).unwrap();
    let mut messages = Vec::new();
    // Equal received dates must remain distinct across page boundaries.
    for (index, day) in [0, 1, 1, 2, 3].into_iter().enumerate() {
        let received = 1_704_067_200_000 + day * 86_400_000;
        let subject = if index == 4 {
            "Unrelated heading"
        } else {
            "Orchard report"
        };
        let (kind, body) = if index == 4 {
            (
                "text/html",
                format!("<style>hiddenneedle</style><p>Orchard caf=C3=A9 harvest {index}</p>"),
            )
        } else {
            ("text/plain", format!("Orchard caf=C3=A9 harvest {index}"))
        };
        let message = format!(
            "From: Gardener <gardener@example.test>\r\nTo: main@example.test\r\nSubject: {subject}\r\nMessage-ID: <query-{index}@example.test>\r\nMIME-Version: 1.0\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{body}\r\n"
        );
        let delivery = postgres_receiving::prepare(
            binding,
            &postgres_receiving::Message {
                bytes: message.as_bytes(),
                hostname: "localhost",
                extended: true,
                header_bytes: 65536,
                received_at: received,
                receipt: SmtpReceipt {
                    peer: "192.0.2.1".parse().unwrap(),
                    tls: ReceiptTls::Tls13,
                    gateway: None,
                    ehlo: "sender.example.test",
                    reverse_path: "sender@example.test",
                    recipients,
                },
            },
            deadline(),
        )
        .unwrap();
        postgres_receiving::publish(request, trust, deadline(), &delivery).unwrap();
        let mut wire = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let wire = td_mta::wire::encode_id(delivery.email(), &mut wire)
            .unwrap()
            .to_owned();
        messages.push((received, wire));
    }
    // Wire IDs use canonical hex, so lexical ID order is the raw-ID tie-break.
    messages.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let ids: Vec<&str> = messages.iter().map(|(_, id)| id.as_str()).collect();
    let check = |reply: Json, position: u64, expected: &[&str]| {
        assert!(reply["ok"].is_true(), "{reply}");
        assert_eq!(reply["position"].as_u64(), Some(position), "{reply}");
        assert_eq!(
            reply["loaded"].as_u64(),
            Some(expected.len() as u64),
            "{reply}"
        );
        let actual: Vec<&str> = reply["emails"]
            .as_array()
            .unwrap()
            .iter()
            .map(|email| email["id"].as_str().unwrap())
            .collect();
        assert_eq!(actual, expected, "{reply}");
    };
    for position in [0, 3, 5] {
        let end = (position + 3).min(ids.len());
        check(
            client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
                "limit":3, "position":position, "search":"oRcHaRd"})),
            position as u64,
            &ids[position..end],
        );
    }
    // This uses the native indexed date path, without the text-search scanner.
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "limit":2, "position":2, "received_after":"2024-01-01T00:00:00Z"})),
        2,
        &ids[2..4],
    );
    for search in ["CAFÉ", "\"café harvest\"", "gardener orchard"] {
        check(
            client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
            "search":search})),
            0,
            &ids,
        );
    }
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"bytes.bin"})),
        0,
        &[original],
    );
    // after is inclusive; before is exclusive, even with a text condition.
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"orchard", "received_after":"2024-01-02T00:00:00Z",
        "received_before":"2024-01-03T00:00:00Z"})),
        0,
        &ids[2..4],
    );
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "received_after":"2024-01-02T00:00:00Z",
        "received_before":"2024-01-02T00:00:00Z"})),
        0,
        &[],
    );
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"orchard absent-token"})),
        0,
        &[],
    );
    let invalid = client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "received_after":"not-a-date"}));
    assert!(
        !invalid["ok"].is_true(),
        "invalid date must not become an empty success: {invalid}"
    );
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"hiddenneedle"})),
        0,
        &[],
    );
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"\"client Hello\""})),
        0,
        &[],
    );
    let invalid = client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"x".repeat(4097)}));
    assert!(!invalid["ok"].is_true(), "oversized operand: {invalid}");
    let moved = client.ask(json!({"command":"move_email", "id":ids[0], "to_mailbox_id":other}));
    assert!(moved["ok"].is_true(), "{moved}");
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"orchard"})),
        0,
        &ids[1..],
    );
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":other,
        "search":"orchard"})),
        0,
        &ids[..1],
    );
    for id in ids {
        let deleted = client.ask(json!({"command":"destroy", "id":id}));
        assert!(deleted["results"][0]["ok"].is_true(), "{deleted}");
    }
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"orchard"})),
        0,
        &[],
    );
    let nested = postgres_receiving::prepare(binding, &postgres_receiving::Message {
        bytes: b"From: sender@example.test\r\nTo: main@example.test\r\nSubject: Nested\r\nMIME-Version: 1.0\r\nContent-Type: message/rfc822\r\n\r\nFrom: inner@example.test\r\nSubject: Orchard\r\n\r\nNested content\r\n",
        hostname: "localhost", extended: true, header_bytes: 65536, received_at: 1_704_067_200_000,
        receipt: SmtpReceipt { peer: "192.0.2.1".parse().unwrap(), tls: ReceiptTls::Tls13,
            gateway: None, ehlo: "sender.example.test", reverse_path: "sender@example.test", recipients },
    }, deadline()).unwrap();
    postgres_receiving::publish(request, trust, deadline(), &nested).unwrap();
    let mut wire = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let wire = td_mta::wire::encode_id(nested.email(), &mut wire).unwrap();
    for search in ["inner@example.test orchard", "nested content"] {
        check(
            client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
            "search":search})),
            0,
            &[wire],
        );
    }
    check(
        client.ask(json!({"command":"query_emails", "mailbox_id":inbox,
        "search":"orchard absent-token"})),
        0,
        &[],
    );
    let deleted = client.ask(json!({"command":"destroy", "id":wire}));
    assert!(deleted["results"][0]["ok"].is_true(), "{deleted}");
}

fn exercise_send(
    client: &mut Client,
    fixture: &Fixture,
    binding: &Binding,
    runtime: &std::path::Path,
    device: td_mta::ids::DeviceId,
    password: &[u8],
) {
    let drafts = runtime.join("drafts");
    fs::create_dir(&drafts).unwrap();
    fs::set_permissions(&drafts, fs::Permissions::from_mode(0o700)).unwrap();
    let path = drafts.join("td-mail-draft-1-2.eml");
    let binary = drafts.join("résumé.bin");
    let forwarded = drafts.join("forwarded.eml");
    let forwarded_bytes =
        "From: sender@example.test\r\nSubject: Original\r\nContent-Transfer-Encoding: 8bit\r\n\r\nForwarded café exact bytes.\r\n".as_bytes();
    fs::write(&binary, [0, 1, 255]).unwrap();
    fs::write(&forwarded, forwarded_bytes).unwrap();
    fs::write(&path, format!("From: main@example.test\nTo: recipient@example.test\nSubject: Real PostgreSQL send\n--text follows this line--\n\nHello from td-mail through PostgreSQL.\n<#part type=\"application/octet-stream\" filename=\"{}\" disposition=\"attachment\" description=\"Pièce jointe\">\n<#/part>\n<#part type=\"message/rfc822\" filename=\"{}\" disposition=\"attachment\">\n<#/part>\n", binary.display(), forwarded.display())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    // Keep a successful draft locally once, then exercise td-mail's retained
    // send record and server reconciliation instead of publishing it twice.
    let blocked_retirement = runtime.join("sent");
    fs::write(&blocked_retirement, b"fixture blocks retirement").unwrap();
    let sent = client.ask(json!({"command":"send_draft","path":path.to_str().unwrap()}));
    assert!(sent["ok"].is_true(), "{sent}");
    let email = sent["email_id"].as_str().unwrap();
    let submission: td_mta::ids::SubmissionId =
        td_mta::wire::Id::parse(sent["submission_id"].as_str().unwrap())
            .unwrap()
            .local()
            .unwrap();
    assert!(path.exists());
    assert!(sent["retired_to"].is_null());
    assert!(sent["warning"].as_str().is_some());
    fs::remove_file(&blocked_retirement).unwrap();
    let reconciled = client.ask(json!({"command":"send_draft","path":path.to_str().unwrap()}));
    assert!(reconciled["ok"].is_true(), "{reconciled}");
    assert_eq!(reconciled["submission_id"], sent["submission_id"]);
    assert_eq!(reconciled["email_id"], sent["email_id"]);
    assert!(!path.exists());
    assert!(std::path::Path::new(reconciled["retired_to"].as_str().unwrap()).is_file());
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "1");
    assert_eq!(sql(fixture, &format!("SELECT submitting_profile||':'||route_name FROM td_mta.submission_routes WHERE submission_id=decode('{submission}','hex')")), format!("{}:personal-smarthost",binding.profile()));
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.keywords WHERE keyword='$draft'"
        ),
        "0"
    );
    assert_eq!(
        sql(fixture, "SELECT address FROM td_mta.recipients"),
        "recipient@example.test"
    );
    let mailboxes = client.ask(json!({"command":"list_mailboxes"}));
    assert!(mailboxes["ok"].is_true(), "{mailboxes}");
    let mailbox = mailboxes["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == reconciled["kept_in"])
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let listing = client.ask(json!({"command":"query_emails","mailbox_id":mailbox,"limit":50}));
    assert!(listing["ok"].is_true(), "{listing}");
    let stored = listing["emails"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"].as_str() == Some(email))
        .unwrap();
    assert_eq!(stored["subject"].as_str(), Some("Real PostgreSQL send"));
    assert!(stored["body"]
        .as_str()
        .unwrap()
        .contains("Hello from td-mail through PostgreSQL."));
    let attachments = stored["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 2, "{stored}");
    for (attachment, name, expected) in [
        (&attachments[0], "résumé.bin", &[0, 1, 255][..]),
        (&attachments[1], "forwarded.eml", forwarded_bytes),
    ] {
        assert_eq!(attachment["name"].as_str(), Some(name), "{attachment}");
        let download = client.ask(
            json!({"command":"download_attachment", "blob_id":attachment["blob_id"], "name":name}),
        );
        assert!(download["ok"].is_true(), "{download}");
        assert_eq!(
            fs::read(download["path"].as_str().unwrap()).unwrap(),
            expected
        );
    }
    // Composition must not turn an expired or another device's upload lease
    // into an account-visible Email, even after bytes were prepared.
    use td_crypto::{Crypto, Digest};
    use td_mta::{
        compose_text::{Address, Message},
        postgres_drafts as drafts_api,
    };
    let source = td_mta::ids::BlobId::parse(&sql(fixture,"SELECT encode(b.id,'hex') FROM td_mta.blobs b JOIN td_mta.leases l ON l.blob_id=b.id AND l.account=b.account WHERE b.length=3")).unwrap();
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(&[0, 1, 255]).unwrap();
    let attachment = drafts_api::Attachment {
        source: drafts_api::AttachmentSource {
            blob: source,
            length: 3,
            digest: hash.finish().unwrap(),
        },
        part: td_mta::compose_text::Attachment {
            name: "copy.bin",
            content_type: "application/octet-stream",
            description: None,
            bytes: &[0, 1, 255],
        },
    };
    let mailbox = td_mta::wire::Id::parse(mailbox).unwrap().local().unwrap();
    let message = Message {
        from: Address {
            name: "",
            email: "main@example.test",
        },
        to: &[],
        cc: &[],
        bcc: &[],
        reply_to: &[],
        subject: "lease refusal",
        message_id: "refused@example.test",
        in_reply_to: &[],
        references: &[],
        sent_at_ms: 0,
        text: "body",
    };
    let clock = td_mta::clock::RuntimeClock::new();
    let make = |device| {
        let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let mut meter = td_mta::admission::work::Meter::new(
            until,
            td_mta::admission::work::Charge {
                io_bytes: 1_000_000,
                output_bytes: 1_000_000,
                records: 100_000,
                unlinks: 0,
            },
        );
        drafts_api::prepare_with_attachments(
            drafts_api::DraftRequest {
                binding,
                device,
                mailbox,
                expected: None,
            },
            &message,
            std::slice::from_ref(&attachment),
            deadline(),
            &mut meter,
            &clock,
        )
        .unwrap()
    };
    let trust = td_crypto::TrustStore::from_pem(&fixture.ca).unwrap();
    let other = td_mta::postgres_devices::prepare_creation(
        binding.epoch(),
        binding.account(),
        "other attachment device",
        2,
    )
    .unwrap();
    td_mta::postgres_devices::publish_creation(&fixture.request(), &trust, deadline(), &other)
        .unwrap();
    let before = sql(fixture, "SELECT count(*) FROM td_mta.emails");
    assert_eq!(
        drafts_api::publish(
            &fixture.request(),
            &trust,
            deadline(),
            &make(other.device_id()),
            other.password()
        ),
        Err(drafts_api::Error::MissingAttachment)
    );
    let prepared = make(device);
    sql(
        fixture,
        &format!("UPDATE td_mta.leases SET expires_at=1 WHERE blob_id=decode('{source}','hex')"),
    );
    assert_eq!(
        drafts_api::publish(&fixture.request(), &trust, deadline(), &prepared, password),
        Err(drafts_api::Error::MissingAttachment)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), before);
}
