// Declared first, and with `#[macro_use]`, so `json!` reaches `mock_jmap`
// too — one copy of the module per test crate, or its `#[macro_export]`
// macros collide.
#[macro_use]
#[allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

mod mock_fetch;
mod mock_jmap;

#[path = "../src/testing.rs"]
mod testing;

use json::Json as Value;
use mock_fetch::MockFetchSocket;
use mock_jmap::MockJmapServer;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

struct CliHarness {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    _server: MockJmapServer,
    // tmc reaches the mock server through the fetch socket, as it reaches
    // every origin. Dropped before the directory that holds it.
    _fetch: MockFetchSocket,
    _runtime_dir: testing::TempDir,
    _config_dir: testing::TempDir,
}

impl CliHarness {
    fn start() -> Self {
        Self::start_with_mail_config("")
    }

    fn start_with_mail_config(mail_config: &str) -> Self {
        Self::start_with_opts(mail_config, false, None, None)
    }

    fn start_with_data_home(data_home: PathBuf) -> Self {
        Self::start_with_opts("", false, None, Some(data_home))
    }

    fn start_with_opts(
        mail_config: &str,
        offline: bool,
        cache_home: Option<PathBuf>,
        data_home: Option<PathBuf>,
    ) -> Self {
        let server = MockJmapServer::start();
        let runtime_dir = testing::tempdir().expect("create runtime dir");
        let fetch = MockFetchSocket::start(runtime_dir.path());
        let config_dir = testing::tempdir().expect("create temp dir");
        let config_path = config_dir.path().join("config.toml");

        let config_content = format!(
            r#"[account.test]
well_known_url = "{}/.well-known/jmap"
username = "test@example.com"
password_command = "echo test"

[mail]
{}
"#,
            server.url(),
            mail_config
        );
        std::fs::write(&config_path, config_content).expect("write config");

        let tmc_bin = env!("CARGO_BIN_EXE_tmc");
        let mut command = Command::new(tmc_bin);
        command
            .arg("--cli")
            .arg(format!("--config={}", config_path.display()))
            .env("XDG_RUNTIME_DIR", fetch.runtime_dir());
        if offline {
            command.arg("--offline");
        }
        if let Some(cache_home) = cache_home {
            command.env("XDG_CACHE_HOME", cache_home);
        }
        if let Some(data_home) = data_home {
            command.env("XDG_DATA_HOME", data_home);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tmc --cli");

        let stdin = child.stdin.take().expect("take stdin");
        let stdout = child.stdout.take().expect("take stdout");
        let reader = BufReader::new(stdout);

        CliHarness {
            child,
            stdin,
            reader,
            _server: server,
            _fetch: fetch,
            _runtime_dir: runtime_dir,
            _config_dir: config_dir,
        }
    }

    fn send(&mut self, cmd: Value) -> Value {
        let line = cmd.to_string();
        writeln!(self.stdin, "{}", line).expect("write to stdin");
        self.stdin.flush().expect("flush stdin");

        let mut response_line = String::new();
        self.reader
            .read_line(&mut response_line)
            .expect("read response");
        json::parse(response_line.trim()).expect("parse response JSON")
    }
}

/// The assertions compare one JSON value against a Rust literal. `Json` has
/// no `PartialEq` with Rust scalars, so these two name the accessor once.
fn text(value: &Value) -> &str {
    value.as_str().expect("a JSON string")
}

fn number(value: &Value) -> i64 {
    value.as_i64().expect("a JSON integer")
}

impl Drop for CliHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn test_list_accounts() {
    let mut h = CliHarness::start();
    let resp = h.send(json!({"command": "list_accounts"}));

    assert!(resp["ok"].is_true());
    let accounts = resp["accounts"].as_array().expect("accounts array");
    assert_eq!(accounts.len(), 1);
    assert_eq!(text(&accounts[0]["name"]), "test");
    assert_eq!(text(&accounts[0]["username"]), "test@example.com");
}

#[test]
fn test_status_before_connect() {
    let mut h = CliHarness::start();
    let resp = h.send(json!({"command": "status"}));

    assert!(resp["ok"].is_true());
    assert!(!resp["connected"].is_true());
    assert!(resp["account"].is_null());
}

#[test]
fn test_connect_and_list_mailboxes() {
    let mut h = CliHarness::start();

    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    let resp = h.send(json!({"command": "list_mailboxes"}));
    assert!(resp["ok"].is_true(), "list_mailboxes failed: {}", resp);

    let mailboxes = resp["mailboxes"].as_array().expect("mailboxes array");
    assert_eq!(mailboxes.len(), 3);

    let names: Vec<&str> = mailboxes
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"INBOX"));
    assert!(names.contains(&"Archive"));
    assert!(names.contains(&"Trash"));
}

#[test]
fn test_query_and_get_email() {
    let mut h = CliHarness::start();

    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    let resp = h.send(json!({
        "command": "query_emails",
        "mailbox_id": "mbox-inbox",
        "limit": 50
    }));
    assert!(resp["ok"].is_true(), "query_emails failed: {}", resp);

    let emails = resp["emails"].as_array().expect("emails array");
    assert_eq!(emails.len(), 4);

    let resp = h.send(json!({"command": "get_email", "id": "email-001"}));
    assert!(resp["ok"].is_true(), "get_email failed: {}", resp);
    assert_eq!(text(&resp["id"]), "email-001");
    assert_eq!(text(&resp["subject"]), "Hello World");
    assert!(resp["body"].as_str().unwrap().contains("body of email 001"));
}

#[test]
fn test_archive_and_delete_work_without_preloading_mailboxes() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());

    let archive_resp = h.send(json!({"command": "archive", "id": "email-002"}));
    assert!(
        archive_resp["ok"].is_true(),
        "archive failed: {}",
        archive_resp
    );

    let delete_resp = h.send(json!({"command": "delete_email", "id": "email-003"}));
    assert!(
        delete_resp["ok"].is_true(),
        "delete failed: {}",
        delete_resp
    );

    let e2 = h.send(json!({"command": "get_email", "id": "email-002", "headers_only": true}));
    let e3 = h.send(json!({"command": "get_email", "id": "email-003", "headers_only": true}));
    let m2 = e2["mailbox_ids"].as_array().unwrap();
    let m3 = e3["mailbox_ids"].as_array().unwrap();
    assert_eq!(text(&m2[0]), "mbox-archive");
    assert_eq!(text(&m3[0]), "mbox-trash");
}

#[test]
fn test_mailbox_id_overrides_and_bulk_commands() {
    let mut h = CliHarness::start_with_mail_config(
        r#"
archive_folder = "not-a-real-folder"
deleted_folder = "also-not-real"
archive_mailbox_id = "mbox-archive"
deleted_mailbox_id = "mbox-trash"
"#,
    );
    assert!(h.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());

    let bulk_archive = h.send(json!({
        "command": "bulk_archive",
        "ids": ["email-001", "email-002"]
    }));
    assert!(
        bulk_archive["ok"].is_true(),
        "bulk_archive failed: {}",
        bulk_archive
    );
    assert_eq!(number(&bulk_archive["succeeded"]), 2);

    let bulk_delete = h.send(json!({
        "command": "bulk_delete_email",
        "ids": ["email-003"]
    }));
    assert!(
        bulk_delete["ok"].is_true(),
        "bulk_delete failed: {}",
        bulk_delete
    );
    assert_eq!(number(&bulk_delete["succeeded"]), 1);

    let inbox = h.send(json!({"command": "query_emails", "mailbox_id": "mbox-inbox", "limit": 50}));
    let ids: Vec<&str> = inbox["emails"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert!(!ids.contains(&"email-001"));
    assert!(!ids.contains(&"email-002"));
    assert!(!ids.contains(&"email-003"));
}

#[test]
fn test_query_emails_date_filters() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());

    let resp = h.send(json!({
        "command": "query_emails",
        "mailbox_id": "mbox-inbox",
        "received_after": "2025-12-01T00:00:00Z",
        "received_before": "2025-12-21T00:00:00Z",
        "limit": 50
    }));
    assert!(
        resp["ok"].is_true(),
        "query with date filters failed: {}",
        resp
    );

    let ids: Vec<&str> = resp["emails"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["email-003", "email-002"]);
}

#[test]
fn test_triage_plan_and_apply() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());

    let plan = h.send(json!({
        "command": "triage_suggest",
        "mailbox_id": "mbox-inbox",
        "received_after": "2025-12-01T00:00:00Z",
        "received_before": "2026-01-01T00:00:00Z",
        "limit": 50
    }));
    assert!(plan["ok"].is_true(), "triage_suggest failed: {}", plan);
    let plan_id = plan["plan_id"].as_str().expect("plan_id");

    let archive_ids: Vec<&str> = plan["archive"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    let trash_ids: Vec<&str> = plan["trash"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();

    assert!(archive_ids.contains(&"email-004"));
    assert!(trash_ids.contains(&"email-003"));

    let apply = h.send(json!({"command": "apply_triage_plan", "plan_id": plan_id}));
    assert!(apply["ok"].is_true(), "apply_triage_plan failed: {}", apply);

    let e4 = h.send(json!({"command": "get_email", "id": "email-004", "headers_only": true}));
    let e3 = h.send(json!({"command": "get_email", "id": "email-003", "headers_only": true}));
    assert_eq!(text(&e4["mailbox_ids"][0]), "mbox-archive");
    assert_eq!(text(&e3["mailbox_ids"][0]), "mbox-trash");
}

#[test]
fn test_mark_read_unread() {
    let mut h = CliHarness::start();

    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    let resp = h.send(json!({"command": "mark_read", "id": "email-002"}));
    assert!(resp["ok"].is_true(), "mark_read failed: {}", resp);
    assert_eq!(text(&resp["action"]), "MarkRead");

    let resp = h.send(json!({"command": "mark_unread", "id": "email-002"}));
    assert!(resp["ok"].is_true(), "mark_unread failed: {}", resp);
    assert_eq!(text(&resp["action"]), "MarkUnread");
}

#[test]
fn test_offline_queue_replay_on_reconnect() {
    let cache_dir = testing::tempdir().expect("create cache dir");

    // Prime cache from an online session.
    {
        let mut online =
            CliHarness::start_with_opts("", false, Some(cache_dir.path().to_path_buf()), None);
        assert!(online.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());
        assert!(online.send(json!({"command": "list_mailboxes"}))["ok"].is_true());
        assert!(online.send(json!({
            "command": "query_emails",
            "mailbox_id": "mbox-inbox",
            "limit": 50
        }))["ok"]
            .is_true());
    }

    // Queue writes offline; they should succeed and update local cache projection.
    {
        let mut offline =
            CliHarness::start_with_opts("", true, Some(cache_dir.path().to_path_buf()), None);
        assert!(offline.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());

        let mark = offline.send(json!({"command": "mark_read", "id": "email-002"}));
        assert!(mark["ok"].is_true(), "offline mark_read failed: {}", mark);

        let archive = offline.send(json!({"command": "archive", "id": "email-001"}));
        assert!(
            archive["ok"].is_true(),
            "offline archive failed: {}",
            archive
        );

        let inbox = offline.send(json!({
            "command": "query_emails",
            "mailbox_id": "mbox-inbox",
            "limit": 50
        }));
        assert!(inbox["ok"].is_true(), "offline query failed: {}", inbox);
        let ids: Vec<&str> = inbox["emails"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["id"].as_str())
            .collect();
        assert!(!ids.contains(&"email-001"));
    }

    // Reconnect online with the same cache; queued ops should replay to server.
    {
        let mut online =
            CliHarness::start_with_opts("", false, Some(cache_dir.path().to_path_buf()), None);
        assert!(online.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());

        let e1 =
            online.send(json!({"command": "get_email", "id": "email-001", "headers_only": true}));
        assert!(
            e1["ok"].is_true(),
            "post-replay get_email e1 failed: {}",
            e1
        );
        assert_eq!(text(&e1["mailbox_ids"][0]), "mbox-archive");

        let e2 =
            online.send(json!({"command": "get_email", "id": "email-002", "headers_only": true}));
        assert!(
            e2["ok"].is_true(),
            "post-replay get_email e2 failed: {}",
            e2
        );
        assert!(e2["is_read"].is_true());
    }
}

#[test]
fn test_download_attachment() {
    let mut h = CliHarness::start();

    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    // Verify email-001 has an attachment
    let resp = h.send(json!({"command": "get_email", "id": "email-001"}));
    assert!(resp["ok"].is_true(), "get_email failed: {}", resp);
    let attachments = resp["attachments"].as_array().expect("attachments array");
    assert_eq!(attachments.len(), 1);
    assert_eq!(text(&attachments[0]["name"]), "test-document.pdf");
    assert_eq!(text(&attachments[0]["blob_id"]), "blob-att-001");

    // Download the attachment
    let resp = h.send(json!({
        "command": "download_attachment",
        "blob_id": "blob-att-001",
        "name": "test-document.pdf",
        "content_type": "application/pdf"
    }));
    assert!(resp["ok"].is_true(), "download_attachment failed: {}", resp);
    assert_eq!(text(&resp["name"]), "test-document.pdf");

    let path_str = resp["path"].as_str().expect("path string");
    let path = Path::new(path_str);
    assert!(
        path.exists(),
        "downloaded file should exist at {}",
        path_str
    );

    let contents = std::fs::read_to_string(path).expect("read downloaded file");
    assert!(
        contents.contains("blob-att-001"),
        "file should contain expected content"
    );

    // Clean up
    let _ = std::fs::remove_file(path);
}

#[test]
fn test_train_spam_and_ham() {
    let data_dir = testing::tempdir().expect("create data dir");
    let mut h = CliHarness::start_with_data_home(data_dir.path().to_path_buf());

    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    // Train a spam message (boolean form).
    let resp = h.send(json!({"command": "train", "id": "email-003", "spam": true}));
    assert!(resp["ok"].is_true(), "train spam failed: {}", resp);
    assert_eq!(text(&resp["id"]), "email-003");
    assert_eq!(text(&resp["trained_as"]), "spam");

    // Train a ham message (string form).
    let resp = h.send(json!({"command": "train", "id": "email-002", "spam": "ham"}));
    assert!(resp["ok"].is_true(), "train ham failed: {}", resp);
    assert_eq!(text(&resp["trained_as"]), "ham");

    // The model is persisted under $XDG_DATA_HOME/tmc/spam-model.json.
    let model_path = data_dir.path().join("tmc").join("spam-model.json");
    let bytes = std::fs::read(&model_path).expect("model file should exist after training");
    let model = json::parse_slice(&bytes).expect("parse model JSON");
    assert_eq!(number(&model["spam_messages"]), 1, "model: {}", model);
    assert_eq!(number(&model["ham_messages"]), 1, "model: {}", model);
    assert!(
        !model["tokens"]
            .as_object()
            .expect("tokens object")
            .is_empty(),
        "model should have learned tokens"
    );
}

#[test]
fn test_train_missing_id_errors() {
    let mut h = CliHarness::start();
    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    let resp = h.send(json!({"command": "train", "spam": true}));
    assert!(!resp["ok"].is_true());
    assert!(resp["error"].as_str().unwrap_or("").contains("id"));
}

/// The one transport is td's fetch service, so a runtime directory without
/// its socket is a named diagnostic rather than a silent failure or a direct
/// connection: td's APPLICATIONS.md §X.
#[test]
fn test_connect_without_fetch_service_names_the_socket() {
    let server = MockJmapServer::start();
    let runtime_dir = testing::tempdir().expect("create runtime dir");
    let config_dir = testing::tempdir().expect("create temp dir");
    let config_path = config_dir.path().join("config.toml");
    let config_content = format!(
        "[account.test]\nwell_known_url = \"{}/.well-known/jmap\"\n\
         username = \"test@example.com\"\npassword_command = \"echo test\"\n",
        server.url()
    );
    std::fs::write(&config_path, config_content).expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tmc"))
        .arg("--cli")
        .arg(format!("--config={}", config_path.display()))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tmc --cli");
    let mut stdin = child.stdin.take().expect("take stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("take stdout"));
    writeln!(
        stdin,
        "{}",
        json!({"command": "connect", "account": "test"})
    )
    .expect("write stdin");
    stdin.flush().expect("flush stdin");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let _ = child.kill();
    let _ = child.wait();

    let resp = json::parse(line.trim()).expect("parse response JSON");
    assert!(!resp["ok"].is_true(), "connect should fail: {}", resp);
    let error = resp["error"].as_str().unwrap_or("");
    let expected = format!(
        "no td-fetch socket at {}/td-fetch/socket: tmc fetches through \
         td's fetch service; on a host, serve one there",
        runtime_dir.path().display()
    );
    assert!(error.contains(&expected), "error was: {}", error);
}
