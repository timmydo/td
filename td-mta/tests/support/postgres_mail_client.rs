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
                .env("XDG_RUNTIME_DIR", root)
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
            requests: 1,
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
    let authentication =
        td_mta::postgres_http_auth::Authentication::new(&loaded, executable, binding, &clock)
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
    let config = runtime.join("mail.toml");
    fs::write(&config, format!("[account.test]\nwell_known_url = \"{origin}/.well-known/jmap\"\nusername = \"user\"\npassword_command = \"printf {}\"\n", std::str::from_utf8(device.password()).unwrap())).unwrap();
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
            .env("TD_TEST_FETCH_SOCKET", &fetch_socket)
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
            authentication: &authentication, origin: Origin::parse(&origin).unwrap(),
            username: "user", resources, timeouts: &timeouts, clock: clock.clone(),
            uploads: Some(&backend), ready: &ready,
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
                exercise_send(&mut client, fixture, binding, &runtime);
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
            }
            let usage_before = sql(fixture, "SELECT body_bytes||':'||blob_count FROM td_mta.account_usage");
            let extra = postgres_receiving::prepare(&cloud_binding, &postgres_receiving::Message {
                bytes: message.as_bytes(), hostname: "localhost", extended: true, header_bytes: 65536,
                received_at: 1_700_000_000_000,
                receipt: SmtpReceipt { peer: "192.0.2.1".parse().unwrap(), tls: ReceiptTls::Tls13, gateway: None,
                    ehlo: "sender.example.test", reverse_path: "sender@example.test", recipients },
            }, deadline()).unwrap();
            postgres_receiving::publish(request, trust, deadline(), &extra).unwrap();
            let mut extra_wire = [0; td_mta::wire::IdKind::WIRE_BYTES];
            let extra_wire = td_mta::wire::encode_id(extra.email(), &mut extra_wire).unwrap();
            let listing = client.ask(json!({"command":"query_emails", "mailbox_id":inbox_wire, "limit":50}));
            assert_eq!(listing["emails"].as_array().unwrap().len(), 2, "{listing}");
            let deleted = client.ask(json!({"command":"destroy", "id":extra_wire}));
            assert!(deleted["results"][0]["ok"].is_true(), "{deleted}");
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
        [td_mta::postgres_mailbox_create::ResultEntry::Created(_)]
    ));
    assert_eq!(confirmed.mailboxes.operation().unwrap(), None);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='Recovered'"
        ),
        "1"
    );
}

fn exercise_send(
    client: &mut Client,
    fixture: &Fixture,
    binding: &Binding,
    runtime: &std::path::Path,
) {
    let drafts = runtime.join("drafts");
    fs::create_dir(&drafts).unwrap();
    fs::set_permissions(&drafts, fs::Permissions::from_mode(0o700)).unwrap();
    let path = drafts.join("td-mail-draft-1-2.eml");
    fs::write(&path, "From: main@example.test\nTo: recipient@example.test\nSubject: Real PostgreSQL send\n\nHello from td-mail through PostgreSQL.\n").unwrap();
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
}
