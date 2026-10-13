//! Real td-mail and td-fetch protocol/TLS path against the native service.
#![allow(clippy::expect_used)]
use super::*;
use crate::{
    admission::{logical::Cell, timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits},
    config::{endpoint::Origin, listener::Kind},
    format::{
        key::Key,
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, MailboxRow, Row},
        Sequence, Table,
    },
    http_auth::DevicePolicy,
    https::{BoundListener, Https},
    ids::{AccountId, BlobId, EmailId, MailboxId, StoreEpoch, ThreadId},
    limits::Limits,
    ownership::SlotState,
    ports::{Deadline, ReadView},
    smtp_receiving::Control,
    store_fs::{
        tests::Fixture, AuxiliaryUsage, BlobSource, CommitRequest, DeviceStore, IndexStore,
        IngressSpool, RequestSpool, StoreCoordinator,
    },
    wire,
};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{mpsc, Mutex},
    time::{Duration, Instant},
};
use td_crypto::Digest;
use td_json::{json, Json};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Client {
    _process: Process,
    input: std::process::ChildStdin,
    output: mpsc::Receiver<String>,
}
impl Client {
    fn start(config: &Path, root: &Path) -> Self {
        let mut process = Process(
            Command::new(
                std::env::var_os("TD_MTA_TEST_MAIL")
                    .expect("qualification builds td-mail/Cargo.toml"),
            )
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
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            _process: process,
            input,
            output: rx,
        }
    }
    fn ask(&mut self, request: Json) -> Json {
        writeln!(self.input, "{request}").unwrap();
        self.input.flush().unwrap();
        let line = self
            .output
            .recv_timeout(Duration::from_secs(30))
            .expect("td-mail response deadline");
        td_json::parse(&line).unwrap()
    }
}
fn wire_id<T: wire::LocalId>(id: T) -> String {
    wire::encode_id(id, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
fn row(value: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 2048];
    let n = value.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
struct RealClock(Instant);
impl Clock for RealClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            monotonic: Tick(self.0.elapsed().as_millis() as u64),
            utc_ms: 1_800_000_000_000,
        })
    }
}

#[test]
#[ignore = "crypto-cargo builds the real td-mail and fetch fixture executables"]
fn td_mail_reads_native_mail_and_downloads_emitted_attachment_ids() {
    let runtime = Fixture::new();
    let devices_dir = Fixture::new();
    let mut devices_root = devices_dir.locked();
    let account = AccountId::from_bytes([0xaa; 16]);
    let inbox = MailboxId::from_bytes([1; 16]);
    let archive = MailboxId::from_bytes([2; 16]);
    let trash = MailboxId::from_bytes([3; 16]);
    let mut registry = DeviceStore::open_or_create(&mut devices_root, account).unwrap();
    let (device, password) = registry
        .create(
            "real-client",
            1,
            &mut td_crypto::SystemEntropy::try_new().unwrap(),
        )
        .unwrap();
    let devices = Mutex::new(registry);
    let material = Material::new();
    std::fs::write(runtime.path.join("root.der"), &material.ca_der).unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let origin = format!("https://localhost:{}", address.port());
    let source = source(false)
        .replace("https://localhost", &origin)
        .replace("127.0.0.1:443", &address.to_string());
    let lease = material.published(&source);
    let resolved = resolved(&source);
    let resources = Limits::default().plan().unwrap();
    let admission = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&admission).unwrap();
    let clock = Arc::new(RealClock(Instant::now()));
    let until = Deadline::after(Tick(0), 600_000).unwrap();
    let data = Fixture::new();
    let ingress = Fixture::new();
    let responses = Fixture::new();
    let mut data_root = data.locked();
    let mut ingress_root = ingress.locked();
    let mut response_root = responses.locked();
    let store = IndexStore::create(
        &mut data_root,
        StoreEpoch::from_bytes([9; 16]),
        clock.clone(),
        2,
        until,
    )
    .unwrap();
    store.create_account(account, until).unwrap();
    let mailbox = row(Row::Mailbox(MailboxRow {
        name: "Inbox",
        parent: None,
        role: Some("inbox"),
        sort_order: 0,
        subscribed: true,
    }));
    let archive_row = row(Row::Mailbox(MailboxRow {
        name: "archive",
        parent: None,
        role: Some("archive"),
        sort_order: 1,
        subscribed: true,
    }));
    let trash_row = row(Row::Mailbox(MailboxRow {
        name: "trash",
        parent: None,
        role: Some("trash"),
        sort_order: 2,
        subscribed: true,
    }));
    store
        .commit(
            &Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: until,
            },
            &[
                Operation::put(Table::Mailboxes, inbox.as_bytes(), &mailbox).unwrap(),
                Operation::put(Table::Mailboxes, archive.as_bytes(), &archive_row).unwrap(),
                Operation::put(Table::Mailboxes, trash.as_bytes(), &trash_row).unwrap(),
            ],
            &mut [],
        )
        .unwrap();
    let opaque = format!("Subject: opaque\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"{}\"\r\n\r\nopaque body", "a".repeat(20 * 1024));
    let html = format!("Subject: html\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n<p>Hello <b>client</b></p><script>hidden</script><p>{} body-only-tail</p>", "visible ".repeat(512));
    let messages = [
        "From: Alice <alice@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: plain\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\nHello reader",
        html.as_str(),
        "Subject: attached\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nAttachment follows\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=bytes.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAAH/\r\n--x--\r\n",
        opaque.as_str(),
    ];
    for (index, message) in messages.iter().enumerate() {
        let byte = 10 + index as u8;
        let email = EmailId::from_bytes([byte; 16]);
        let blob = BlobId::from_bytes([byte; 16]);
        let thread = ThreadId::from_bytes([if byte == 11 { 10 } else { byte }; 16]);
        let mut hash = Provider.sha256().unwrap();
        hash.update(message.as_bytes()).unwrap();
        let blob_row = row(Row::Blob(BlobRow {
            length: message.len() as u64,
            digest: hash.finish().unwrap(),
            created_at: 0,
        }));
        let email_row = row(Row::Email(EmailRow {
            blob,
            thread,
            received_at: index as i64 * 1000,
            origin: EmailOrigin::Jmap,
        }));
        let mut membership = [0; 32];
        Key::Membership(email, inbox)
            .encode(&mut membership)
            .unwrap();
        store
            .commit(
                &Provider,
                CommitRequest {
                    account,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(index as u64 + 1),
                    utc_ms: 0,
                    deadline: until,
                },
                &[
                    Operation::put(Table::Blobs, blob.as_bytes(), &blob_row).unwrap(),
                    Operation::put(Table::Threads, thread.as_bytes(), &[]).unwrap(),
                    Operation::put(Table::Emails, email.as_bytes(), &email_row).unwrap(),
                    Operation::put(Table::Memberships, &membership, &[]).unwrap(),
                ],
                &mut [BlobSource {
                    id: blob,
                    source: &mut message.as_bytes(),
                }],
            )
            .unwrap();
    }
    let spool = IngressSpool::open(&mut ingress_root, &resources, clock.clone(), until).unwrap();
    let response_spool = RequestSpool::open(&mut response_root, clock.clone(), until).unwrap();
    let mut states = [const { SlotState::EMPTY }; 8];
    let mut cells = [const { Cell::EMPTY }; 8];
    let coordinator = StoreCoordinator::new(
        store,
        &admission,
        AuxiliaryUsage {
            sort_bytes: 0,
            response_bytes: 0,
            cache_bytes: 0,
            log_bytes: 0,
            cold_bytes: 0,
        },
        &mut states,
        &mut cells,
        DevicePolicy::new(&devices, clock.as_ref(), 1),
        until,
    )
    .unwrap();
    let transfers = crate::https_upload::Service {
        resources: &resources,
        coordinator: &coordinator,
        spool: &spool,
        account,
        maximum: resources.limits().message_bytes as u64,
        clock: clock.as_ref(),
        timeouts: &timeouts,
    };
    let session = crate::jmap_session::Session::mail_access(
        Origin::parse(&origin).unwrap(),
        crate::config::identities::AccountView {
            id: account,
            username: "private-user",
            name: "Private",
        },
        &resources,
        crate::jmap_session::Concurrency {
            uploads: 1,
            requests: 1,
        },
    )
    .unwrap();
    let api = crate::https_api::Service {
        transfers: &transfers,
        coordinator: &coordinator,
        spool: &response_spool,
        policy: DevicePolicy::new(&devices, clock.as_ref(), 1),
        resources: &resources,
        session: &session,
        response_maximum: admission.disk().response_bytes,
        clock: clock.as_ref(),
        timeouts: &timeouts,
    };
    let handshakes = HandshakePool::new(2).unwrap();
    let ready = AtomicBool::new(false);
    let control = Control::default();
    let config = runtime.path.join("mail.toml");
    std::fs::write(&config, format!("[account.test]\nwell_known_url = \"{origin}/.well-known/jmap\"\nusername = \"private-user\"\npassword_command = \"printf {}\"\n", std::str::from_utf8(password.expose()).unwrap())).unwrap();
    let fetch_socket = runtime.path.join("td-fetch/socket");
    let fetch = Process(
        Command::new(
            std::env::var_os("TD_MTA_TEST_FETCH").expect("qualification builds net/Cargo.toml"),
        )
        .args([
            "--exact",
            "fetchd::tests::mail_read_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("TD_TEST_FETCH_ROOT", runtime.path.join("root.der"))
        .env("TD_TEST_FETCH_SOCKET", &fetch_socket)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap(),
    );
    let wait_until = Instant::now() + Duration::from_secs(10);
    while !fetch_socket.exists() {
        assert!(Instant::now() < wait_until, "fetch fixture startup");
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = resolved.candidate().with_graph(|records, graph| {
        let rows = records.view(graph).unwrap().listeners().unwrap();
        let row = (0..rows.len())
            .filter_map(|i| rows.listener(i).unwrap())
            .find(|r| r.kind == Kind::Https)
            .unwrap();
        let listeners = [BoundListener::new(socket, row, &lease).unwrap()];
        let server = Https {
            uploads: Some(&api),
            listeners: &listeners,
            policies: &lease,
            handshakes: &handshakes,
            devices: &devices,
            origin: Origin::parse(&origin).unwrap(),
            username: "private-user",
            resources: &resources,
            timeouts: &timeouts,
            clock: clock.clone(),
            ready: &ready,
        };
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| server.run(&control));
            struct Stop<'a>(&'a Control);
            impl Drop for Stop<'_> {
                fn drop(&mut self) {
                    self.0.stop();
                }
            }
            let stop = Stop(&control);
            while !ready.load(Ordering::Acquire) {
                assert!(Instant::now() < wait_until);
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut client = Client::start(&config, &runtime.path);
            let connected = client.ask(json!({"command":"connect", "account":"test"}));
            assert!(connected["ok"].is_true(), "{connected}");
            let mailboxes = client.ask(json!({"command":"list_mailboxes"}));
            assert!(mailboxes["ok"].is_true(), "{mailboxes}");
            let listed = mailboxes["mailboxes"].as_array().unwrap();
            assert_eq!(listed.len(), 3);
            assert_eq!(listed[0]["id"].as_str(), Some(wire_id(inbox).as_str()));
            let listing = client.ask(json!({"command":"query_emails", "mailbox_id":wire_id(inbox), "limit":50}));
            assert!(listing["ok"].is_true(), "{listing}");
            let emails = listing["emails"].as_array().unwrap();
            assert_eq!(emails.len(), 4, "{listing}");
            let find = |byte| emails.iter().find(|e| e["id"].as_str() == Some(wire_id(EmailId::from_bytes([byte;16])).as_str())).unwrap();
            assert_eq!(find(10)["subject"].as_str(), Some("plain"));
            assert_eq!(find(10)["body"].as_str(), Some("Hello reader"));
            assert!(find(11)["preview"].as_str().unwrap().contains("Hello client"));
            assert!(!find(11)["preview"].as_str().unwrap().contains("hidden"));
            assert!(!find(11)["preview"].as_str().unwrap().contains("body-only-tail"));
            assert!(find(11)["body"].as_str().unwrap().contains("body-only-tail"));
            assert!(find(13)["unreadable"].is_true());
            let attachment = &find(12)["attachments"][0];
            let download = client.ask(json!({"command":"download_attachment", "blob_id":attachment["blob_id"], "name":"bytes.bin"}));
            assert!(download["ok"].is_true(), "{download}");
            assert_eq!(std::fs::read(download["path"].as_str().unwrap()).unwrap(), [0, 1, 255]);
            let raw = client.ask(json!({"command":"download_attachment", "blob_id":find(13)["blob_id"], "name":"opaque.eml", "content_type":"message/rfc822"}));
            assert!(raw["ok"].is_true(), "{raw}");
            assert_eq!(std::fs::read(raw["path"].as_str().unwrap()).unwrap(), opaque.as_bytes());
            let thread = client.ask(json!({"command":"get_thread", "thread_id":find(10)["thread_id"]}));
            assert!(thread["ok"].is_true(), "{thread}");
            assert_eq!(thread["emails"].as_array().unwrap().len(), 2);
            assert_eq!(thread["emails"][0]["id"], find(10)["id"]);
            assert_eq!(thread["emails"][0]["body"].as_str(), Some("Hello reader"));
            assert_eq!(thread["emails"][1]["id"], find(11)["id"]);
            assert!(thread["emails"][1]["body"].as_str().unwrap().contains("body-only-tail"));
            let target = wire_id(EmailId::from_bytes([10; 16]));
            for (command, seen, flagged) in [
                ("mark_read", true, false),
                ("flag", true, true),
                ("mark_unread", false, true),
                ("unflag", false, false),
            ] {
                let changed = client.ask(json!({"command":command,"id":target}));
                assert!(changed["ok"].is_true(), "{changed}");
                let listing = client.ask(json!({"command":"query_emails", "mailbox_id":wire_id(inbox)}));
                assert!(listing["ok"].is_true(), "{listing}");
                let item = listing["emails"]
                    .as_array().unwrap().iter()
                    .find(|e| e["id"].as_str() == Some(target.as_str()))
                    .unwrap();
                assert_eq!(item["is_read"].as_bool(), Some(seen), "{item}");
                assert_eq!(item["is_flagged"].as_bool(), Some(flagged), "{item}");
                let mut retained = coordinator.retain_response(&response_spool, until).unwrap();
                let phase = Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap();
                let mut view = retained.method_view(account, phase).unwrap();
                let mut scratch = [0; 128];
                for (keyword, present) in [("$seen", seen), ("$flagged", flagged)] {
                    let key = Key::Keyword(EmailId::from_bytes([10; 16]), keyword);
                    assert_eq!(view.get(key, &mut scratch).unwrap().is_some(), present);
                }
            }
            for (command, destination) in [("archive", archive), ("delete_email", trash), ("move_email", inbox)] {
                let input = if command == "move_email" {
                    json!({"command":command, "id":target, "to_mailbox_id":wire_id(destination)})
                } else { json!({"command":command, "id":target}) };
                let moved = client.ask(input);
                assert!(moved["ok"].is_true(), "{moved}");
                let listing = client.ask(json!({"command":"query_emails", "mailbox_id":wire_id(destination)}));
                assert!(listing["ok"].is_true(), "{listing}");
                assert!(listing["emails"].as_array().unwrap().iter().any(|item| item["id"].as_str() == Some(target.as_str())));
                let mut retained = coordinator.retain_response(&response_spool, until).unwrap();
                let phase = Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap();
                let mut view = retained.method_view(account, phase).unwrap();
                assert_eq!(view.email_mailboxes(EmailId::from_bytes([10; 16]), None, 16).unwrap().values, [destination]);
            }
            devices.lock().unwrap().revoke(device, 2).unwrap();
            let revoked = client.ask(json!({"command":"query_emails", "mailbox_id":wire_id(inbox)}));
            assert!(!revoked["ok"].is_true(), "{revoked}");
            assert_eq!(revoked["error"].as_str(), Some(format!("HTTP error: {origin}/jmap/api: status code 401").as_str()), "{revoked}");
            // The CLI acknowledges durable local intent, including remote refusal.
            let queued = client.ask(json!({"command":"mark_read", "id":target}));
            assert!(queued["ok"].is_true(), "{queued}");
            let queued = client.ask(json!({"command":"archive", "id":target}));
            assert!(queued["ok"].is_true(), "{queued}");
            {
                let mut retained = coordinator.retain_response(&response_spool, until).unwrap();
                let phase = Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap();
                let mut view = retained.method_view(account, phase).unwrap();
                let key = Key::Keyword(EmailId::from_bytes([10; 16]), "$seen");
                assert!(view.get(key, &mut [0; 128]).unwrap().is_none());
                assert_eq!(view.email_mailboxes(EmailId::from_bytes([10; 16]), None, 16).unwrap().values, [inbox]);
            }
            drop(client);
            let (_, replacement) = devices.lock().unwrap().create(
                "replacement", 3, &mut td_crypto::SystemEntropy::try_new().unwrap()
            ).unwrap();
            std::fs::write(&config, format!(
                "[account.test]\nwell_known_url = \"{origin}/.well-known/jmap\"\nusername = \"private-user\"\npassword_command = \"printf {}\"\n",
                std::str::from_utf8(replacement.expose()).unwrap()
            )).unwrap();
            let mut client = Client::start(&config, &runtime.path);
            let log_path = runtime.path.join("state/td-mail/td-mail.log");
            let mut log_start = std::fs::read(&log_path).map_or(0, |bytes| bytes.len());
            for attempt in 0..3 {
                let connected = client.ask(json!({"command":"connect", "account":"test"}));
                assert!(connected["ok"].is_true(), "{connected}");
                // Fence this replay attempt; transient refusal may leave work queued.
                let listing = client.ask(json!({"command":"list_mailboxes"}));
                assert!(listing["ok"].is_true(), "{listing}");
                let phase = Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap();
                let complete = loop {
                    let observed = (|| -> Result<bool, crate::store_fs::ResponseError> {
                        let mut retained = coordinator.retain_response(&response_spool, until)?;
                        let mut view = retained.method_view(account, phase)?;
                        let key = Key::Keyword(EmailId::from_bytes([10; 16]), "$seen");
                        Ok(view.get(key, &mut [0; 128])?.is_some()
                            && view.email_mailboxes(EmailId::from_bytes([10; 16]), None, 16)?.values == [archive])
                    })();
                    match observed {
                        Ok(complete) => break complete,
                        Err(crate::store_fs::ResponseError::Store(crate::ports::Error::Busy)) => {
                            assert!(clock.sample().unwrap().monotonic < phase.tick());
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("native replay probe failed: {error}"),
                    }
                };
                if complete { break; }
                let log = std::fs::read(&log_path).unwrap();
                let tail = std::str::from_utf8(&log[log_start..]).unwrap();
                assert!(tail.lines().any(|line| line.contains("[Backend] queue replay stopped at seq=")
                    && (line.contains("status code 429") || line.contains("status code 503"))),
                    "replay may reconnect only after a verified HTTP 429 or 503");
                log_start = log.len();
                assert!(attempt < 2, "queued flag and archive replay did not commit after three attempts");
                std::thread::sleep(Duration::from_secs(1));
            }
            drop(client);
            drop(stop);
            assert!(worker.join().unwrap().is_ok());
        });
    });
    result.unwrap();
    drop(fetch);
}
