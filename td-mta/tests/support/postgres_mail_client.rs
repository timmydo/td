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
        let mut process = Process(
            Command::new(std::env::var_os("TD_MTA_TEST_MAIL").unwrap())
                .arg("--cli")
                .arg(format!("--config={}", config.display()))
                .env("XDG_RUNTIME_DIR", root)
                .env("HOME", root)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_STATE_HOME", root.join("state"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_DOWNLOAD_DIR", root.join("downloads"))
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

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    configuration: &ResolvedProfile,
    policies: &GenerationLease<TlsPolicies>,
    socket: TcpListener,
) {
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
    let session = td_mta::jmap_session::Session::mail_access(
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
    let backend = td_mta::postgres_http_api::Service {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
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
    assert_eq!(responses.used().unwrap(), 0);
    assert!(!responses.admission_stopped());
}
