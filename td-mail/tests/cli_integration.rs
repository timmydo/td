mod mock_fetch;
mod mock_jmap;

#[path = "../src/testing.rs"]
mod testing;

use mock_fetch::MockFetchSocket;
use mock_jmap::MockJmapServer;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use td_json::json;
use td_json::Json as Value;

struct CliHarness {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    _server: MockJmapServer,
    // td-mail reaches the mock server through the fetch socket, as it reaches
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
        Self::start_with_opts("", mail_config, false, None, None)
    }

    fn start_with_data_home(data_home: PathBuf) -> Self {
        Self::start_with_opts("", "", false, None, Some(data_home))
    }

    /// `account_config` is added to `[account.test]`, `mail_config` to
    /// `[mail]`.
    fn start_with_opts(
        account_config: &str,
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
{}

[mail]
{}
"#,
            server.url(),
            account_config,
            mail_config
        );
        std::fs::write(&config_path, config_content).expect("write config");

        let mail_bin = env!("CARGO_BIN_EXE_td-mail");
        let mut command = Command::new(mail_bin);
        command
            .arg("--cli")
            .arg(format!("--config={}", config_path.display()))
            .env("XDG_RUNTIME_DIR", fetch.runtime_dir())
            .env("XDG_STATE_HOME", config_dir.path().join("state"))
            .env("XDG_CACHE_HOME", config_dir.path().join("cache"))
            .env("XDG_DOWNLOAD_DIR", config_dir.path().join("downloads"));
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
            .expect("spawn td-mail --cli");

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

    /// td-mail ended and started again with the same configuration,
    /// against the same server and fetch socket.
    fn restart(&mut self) {
        self.restart_with_offline(false);
    }

    fn restart_with_offline(&mut self, offline: bool) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-mail"));
        if offline {
            command.arg("--offline");
        }
        let mut child = command
            .arg("--cli")
            .arg(format!(
                "--config={}",
                self._config_dir.path().join("config.toml").display()
            ))
            .env("XDG_RUNTIME_DIR", self._fetch.runtime_dir())
            .env("XDG_STATE_HOME", self._config_dir.path().join("state"))
            .env("XDG_CACHE_HOME", self._config_dir.path().join("cache"))
            .env(
                "XDG_DOWNLOAD_DIR",
                self._config_dir.path().join("downloads"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn td-mail --cli");
        self.stdin = child.stdin.take().expect("take stdin");
        self.reader = BufReader::new(child.stdout.take().expect("take stdout"));
        self.child = child;
    }

    /// The mock server, for what it received.
    fn server(&self) -> &MockJmapServer {
        &self._server
    }

    fn send(&mut self, cmd: Value) -> Value {
        let line = cmd.to_string();
        writeln!(self.stdin, "{}", line).expect("write to stdin");
        self.stdin.flush().expect("flush stdin");

        let mut response_line = String::new();
        self.reader
            .read_line(&mut response_line)
            .expect("read response");
        td_json::parse(response_line.trim()).expect("parse response JSON")
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
    assert_eq!(mailboxes.len(), 5);

    let names: Vec<&str> = mailboxes
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"INBOX"));
    assert!(names.contains(&"Archive"));
    assert!(names.contains(&"Drafts"));
    assert!(names.contains(&"Sent"));
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

/// The connected account's own folders, ids and From replace `[mail]`'s,
/// whose folder names would resolve to nothing.
#[test]
fn test_account_overrides_mail_settings_on_connect() {
    let mut h = CliHarness::start_with_opts(
        r#"reply_from = "Work <test@example.com>"
archive_folder = "not-a-real-folder"
archive_mailbox_id = "mbox-archive"
deleted_mailbox_id = "mbox-trash""#,
        r#"reply_from = "Shared <shared@example.com>"
archive_folder = "also-not-real"
deleted_folder = "nor-this""#,
        false,
        None,
        None,
    );
    assert!(h.send(json!({"command": "connect", "account": "test"}))["ok"].is_true());
    let draft = h.send(json!({"command": "compose_draft"}));
    assert!(
        text(&draft["draft"]).starts_with("From: Work <test@example.com>\n"),
        "{}",
        draft
    );
    let archive = h.send(json!({"command": "bulk_archive", "ids": ["email-001"]}));
    assert_eq!(number(&archive["succeeded"]), 1, "{}", archive);
    let delete = h.send(json!({"command": "bulk_delete_email", "ids": ["email-002"]}));
    assert_eq!(number(&delete["succeeded"]), 1, "{}", delete);
}

#[test]
fn search_keeps_mailbox_dates_paging_and_reports_empty_results() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let response = h.send(json!({
        "command":"query_emails", "mailbox_id":"mbox-inbox",
        "search":"Meeting", "received_after":"2025-12-01T00:00:00Z",
        "received_before":"2025-12-21T00:00:00Z", "limit":1,
        "headers_only":true
    }));
    assert!(response["ok"].is_true(), "{response}");
    assert_eq!(response["emails"].as_array().unwrap().len(), 1);
    assert_eq!(response["emails"][0]["id"].as_str(), Some("email-002"));
    let calls = h.server().query_calls();
    let call = calls.last().unwrap();
    assert_eq!(
        call["filter"],
        json!({"operator":"AND", "conditions":[
            {"inMailbox":"mbox-inbox"}, {"text":"Meeting"},
            {"after":"2025-12-01T00:00:00Z"}, {"before":"2025-12-21T00:00:00Z"}
        ]})
    );
    assert_eq!(call["position"].as_u64(), Some(0));
    assert_eq!(call["limit"].as_u64(), Some(1));
    for (query, position) in [("Meeting", 1), ("absent-search-marker", 0)] {
        let response = h.send(json!({
            "command":"query_emails", "mailbox_id":"mbox-inbox",
            "search":query, "position":position, "headers_only":true
        }));
        assert!(response["ok"].is_true(), "{response}");
        assert!(
            response["emails"].as_array().unwrap().is_empty(),
            "{response}"
        );
        let calls = h.server().query_calls();
        assert_eq!(calls.last().unwrap()["position"].as_u64(), Some(position));
    }
}

#[test]
fn search_refusal_preserves_server_reason_without_empty_success_or_retry() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    for (kind, description) in [
        ("serverFail", "storageWorkLimit"),
        ("serverFail", "messageInterpretation"),
        ("unsupportedFilter", "unsupported search operand"),
    ] {
        h.server().refuse_query(kind, description);
        let before = h.server().query_calls().len();
        let response = h.send(json!({
            "command":"query_emails", "mailbox_id":"mbox-inbox",
            "search":"private-search-marker", "headers_only":true
        }));
        assert!(!response["ok"].is_true(), "{response}");
        let error = response["error"].as_str().unwrap();
        assert!(
            error.contains(&format!("Email/query failed: {kind} {description}")),
            "{error}"
        );
        assert_eq!(h.server().query_calls().len(), before + 1);
        assert!(h.server().read_calls().is_empty());
    }
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
fn read_only_discovery_keeps_reads_and_refuses_remote_mutations() {
    let mut h = CliHarness::start();
    h.server().read_only();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"get_email", "id":"email-002"}))["ok"].is_true());
    let before = h.server().email_state("email-002");
    for command in [
        json!({"command":"mark_read", "id":"email-002"}),
        json!({"command":"flag", "id":"email-002"}),
        json!({"command":"move_email", "id":"email-002", "to_mailbox_id":"mbox-archive"}),
        json!({"command":"create_mailbox", "name":"Refused"}),
    ] {
        let answer = h.send(command);
        assert!(!answer["ok"].is_true(), "{answer}");
        assert!(answer.to_string().contains("read-only"), "{answer}");
    }
    let destroyed = h.send(json!({"command":"destroy", "ids":["email-002"]}));
    assert!(destroyed["ok"].is_true());
    assert!(!destroyed["results"][0]["ok"].is_true());
    assert!(destroyed["results"][0]["error"]
        .as_str()
        .unwrap()
        .contains("read-only"));
    assert_eq!(h.server().email_state("email-002"), before);
    assert!(h.server().update_calls().is_empty());
    assert!(h.server().destroy_calls().is_empty());
    assert!(h.server().created_mailboxes().is_empty());
    assert!(h.send(json!({"command":"download_attachment", "blob_id":"blob-att-001", "name":"test-document.pdf", "content_type":"application/pdf"}))["ok"].is_true());
}

#[test]
fn read_only_policy_survives_offline_restart_without_projection() {
    let mut h = CliHarness::start();
    h.server().read_only();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    let before = h.send(json!({"command":"get_email", "id":"email-002"}));
    assert!(before["ok"].is_true());
    h.restart_with_offline(true);
    let connected = h.send(json!({"command":"connect", "account":"test"}));
    assert!(
        connected["ok"].is_true() && connected["offline"].is_true(),
        "{connected}"
    );
    let refused = h.send(json!({"command":"mark_read", "id":"email-002"}));
    assert!(!refused["ok"].is_true(), "{refused}");
    assert!(refused.to_string().contains("read-only"), "{refused}");
    let after = h.send(json!({"command":"get_email", "id":"email-002"}));
    assert!(after["ok"].is_true(), "{after}");
    assert_eq!(after["email"]["keywords"], before["email"]["keywords"]);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.server().update_calls().is_empty());
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
            CliHarness::start_with_opts("", "", false, Some(cache_dir.path().to_path_buf()), None);
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
            CliHarness::start_with_opts("", "", true, Some(cache_dir.path().to_path_buf()), None);
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
            CliHarness::start_with_opts("", "", false, Some(cache_dir.path().to_path_buf()), None);
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

    // The model is persisted under $XDG_DATA_HOME/td-mail/spam-model.json.
    let model_path = data_dir.path().join("td-mail").join("spam-model.json");
    let bytes = std::fs::read(&model_path).expect("model file should exist after training");
    let model = td_json::parse_slice(&bytes).expect("parse model JSON");
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

    let mut child = Command::new(env!("CARGO_BIN_EXE_td-mail"))
        .arg("--cli")
        .arg(format!("--config={}", config_path.display()))
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn td-mail --cli");
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

    let resp = td_json::parse(line.trim()).expect("parse response JSON");
    assert!(!resp["ok"].is_true(), "connect should fail: {}", resp);
    let error = resp["error"].as_str().unwrap_or("");
    let expected = format!(
        "no td-fetch socket at {}/td-fetch/socket: td-mail fetches through \
         td's fetch service; on a host, ./install-apps installs a td-mail that serves one, or \
         td-net fetchd run --socket PATH",
        runtime_dir.path().display()
    );
    assert!(error.contains(&expected), "error was: {}", error);
}

/// `attach_file` copies a file into the draft's sidecar under the
/// connected server's `maxSizeUpload` and appends its tag; a missing draft
/// and a file past the limit are refused with nothing copied, and a draft
/// that cannot take the tag has the copy and its new sidecar removed.
#[test]
fn test_attach_file_copies_under_the_servers_limit() {
    let mut h = CliHarness::start();
    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    let state = testing::tempdir().expect("state dir");
    let drafts = state.path().join("td-mail/drafts");
    std::fs::create_dir_all(&drafts).expect("drafts dir");
    let draft = drafts.join("td-mail-draft-4-4.eml");
    let template =
        "From: me@example.com\nTo: you@example.com\nSubject: s\n--text follows this line--\nhi";
    std::fs::write(&draft, template).expect("draft");
    let files = testing::tempdir().expect("files dir");
    let small = files.path().join("notes.txt");
    std::fs::write(&small, "some notes").expect("small file");
    // One byte past the mock's maxSizeUpload.
    let large = files.path().join("large.bin");
    std::fs::write(&large, vec![0u8; 1_000_001]).expect("large file");

    // No draft there: refused, nothing copied.
    let missing = drafts.join("td-mail-draft-5-5.eml");
    let resp = h.send(json!({
        "command": "attach_file",
        "path": missing.to_string_lossy(),
        "file": small.to_string_lossy()
    }));
    assert!(!resp["ok"].is_true(), "attached to no draft: {}", resp);
    assert!(
        text(&resp["error"]).contains("is not a draft file"),
        "refused before the copy: {}",
        resp
    );
    assert!(!drafts.join("td-mail-att-5-5").exists(), "no sidecar made");

    // Past the server's limit: refused before anything is copied.
    let resp = h.send(json!({
        "command": "attach_file",
        "path": draft.to_string_lossy(),
        "file": large.to_string_lossy()
    }));
    assert!(
        !resp["ok"].is_true(),
        "a file past the limit attached: {}",
        resp
    );
    assert!(
        text(&resp["error"]).contains("1000000"),
        "the refusal cites the server's limit: {}",
        resp
    );
    assert_eq!(std::fs::read_to_string(&draft).expect("draft"), template);
    assert!(!drafts.join("td-mail-att-4-4").exists(), "no sidecar made");

    // A draft that is not text cannot take the tag: the copy is made,
    // then removed with the sidecar it made.
    let binary = drafts.join("td-mail-draft-6-6.eml");
    std::fs::write(&binary, b"\xff\xfe").expect("binary draft");
    let resp = h.send(json!({
        "command": "attach_file",
        "path": binary.to_string_lossy(),
        "file": small.to_string_lossy()
    }));
    assert!(!resp["ok"].is_true(), "a binary draft took a tag: {}", resp);
    assert!(
        text(&resp["error"]).contains("could not take the tag"),
        "refused after the copy: {}",
        resp
    );
    assert_eq!(std::fs::read(&binary).expect("draft"), b"\xff\xfe");
    assert!(
        !drafts.join("td-mail-att-6-6").exists(),
        "the copy and its sidecar removed"
    );

    // A replace that fails before its rename (the drafts folder not
    // writable) leaves the draft read back without the tag: the copy is
    // removed, the sidecar this call did not make kept. Root writes
    // regardless, so not as root.
    let root =
        std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").expect("/proc/self"))
            == 0;
    if !root {
        use std::os::unix::fs::PermissionsExt;
        let locked = state.path().join("td-mail/locked");
        let held = locked.join("td-mail-att-7-7");
        std::fs::create_dir_all(&held).expect("locked sidecar");
        std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o700))
            .expect("private sidecar");
        let draft = locked.join("td-mail-draft-7-7.eml");
        std::fs::write(&draft, template).expect("locked draft");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))
            .expect("lock drafts");
        let resp = h.send(json!({
            "command": "attach_file",
            "path": draft.to_string_lossy(),
            "file": small.to_string_lossy(),
            "attachment_dir": held.to_string_lossy()
        }));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock drafts");
        assert!(!resp["ok"].is_true(), "a locked draft took a tag: {}", resp);
        assert!(
            text(&resp["error"]).contains("could not take the tag"),
            "refused after the copy: {}",
            resp
        );
        assert_eq!(std::fs::read_to_string(&draft).expect("draft"), template);
        assert_eq!(
            std::fs::read_dir(&held).expect("sidecar kept").count(),
            0,
            "the copy removed"
        );
    }

    let resp = h.send(json!({
        "command": "attach_file",
        "path": draft.to_string_lossy(),
        "file": small.to_string_lossy()
    }));
    assert!(resp["ok"].is_true(), "attach_file failed: {}", resp);
    let sidecar = drafts.join("td-mail-att-4-4");
    assert_eq!(Path::new(text(&resp["attachment_dir"])), sidecar);
    let copy = sidecar.join("notes.txt");
    assert_eq!(Path::new(text(&resp["copy"])), copy);
    assert_eq!(text(&resp["name"]), "notes.txt");
    assert_eq!(number(&resp["bytes"]), 10);
    assert_eq!(std::fs::read_to_string(&copy).expect("copy"), "some notes");
    let written = std::fs::read_to_string(&draft).expect("draft");
    assert!(
        written.starts_with(&format!("{template}\n<#part "))
            && written.contains(&format!("filename=\"{}\"", copy.display()))
            && written.ends_with("<#/part>\n"),
        "the tag appended: {written}"
    );
}

/// `send_draft` parses the retained draft, uploads its attachment, creates
/// the message in Drafts and submits it in one request under the account's
/// identity, the server then moving it to Sent, read and no longer a
/// draft; the draft and its sidecar retire to `sent` beside `drafts`. A
/// From nobody sends as, and a recipient the server refuses, are errors
/// naming why, and the draft stays where it was.
#[test]
fn test_send_draft_submits_through_jmap_and_retires_the_draft() {
    let mut h = CliHarness::start();
    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);

    let state = testing::tempdir().expect("state dir");
    let drafts = state.path().join("td-mail/drafts");
    std::fs::create_dir_all(&drafts).expect("drafts dir");
    let sidecar = drafts.join("td-mail-att-9-9");
    std::fs::create_dir_all(&sidecar).expect("sidecar dir");
    let attachment = sidecar.join("hello.txt");
    std::fs::write(&attachment, "hello attachment").expect("attachment");
    let draft = drafts.join("td-mail-draft-9-9.eml");
    std::fs::write(
        &draft,
        format!(
            "From: Test User <test@example.com>\nTo: Bob <bob@example.com>\nCc: carol@example.com\nSubject: Sent from td-mail\n--text follows this line--\n\nHello Bob.\n\n<#part type=\"text/plain\" filename=\"{}\" disposition=\"attachment\">\n<#/part>\n",
            attachment.display()
        ),
    )
    .expect("draft");

    let resp = h.send(json!({
        "command": "send_draft",
        "path": draft.to_string_lossy(),
        "attachment_dir": sidecar.to_string_lossy()
    }));
    assert!(resp["ok"].is_true(), "send_draft failed: {}", resp);
    assert_eq!(text(&resp["email_id"]), "email-created-1");
    assert_eq!(text(&resp["submission_id"]), "sub-1");
    assert_eq!(text(&resp["kept_in"]), "Sent");
    let retired = Path::new(text(&resp["retired_to"]));
    assert_eq!(
        retired,
        state.path().join("td-mail/sent/td-mail-draft-9-9.eml")
    );
    assert!(retired.exists() && !draft.exists(), "the draft moved");
    assert!(
        state
            .path()
            .join("td-mail/sent/td-mail-att-9-9/hello.txt")
            .exists()
            && !sidecar.exists(),
        "the sidecar moved with it"
    );

    // What the server saw: the upload, the create and the submission.
    let uploads = h.server().uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].1, "text/plain");
    assert_eq!(uploads[0].2, b"hello attachment");
    let created = h.server().created_emails();
    assert_eq!(created.len(), 1);
    let (id, object) = &created[0];
    assert_eq!(id, "email-created-1");
    assert_eq!(text(&object["from"][0]["email"]), "test@example.com");
    assert_eq!(text(&object["from"][0]["name"]), "Test User");
    assert_eq!(text(&object["to"][0]["email"]), "bob@example.com");
    assert_eq!(text(&object["to"][0]["name"]), "Bob");
    assert_eq!(text(&object["cc"][0]["email"]), "carol@example.com");
    assert!(object["bcc"].is_null());
    assert_eq!(text(&object["subject"]), "Sent from td-mail");
    assert_eq!(text(&object["bodyValues"]["text"]["value"]), "Hello Bob.\n");
    assert_eq!(text(&object["attachments"][0]["blobId"]), &uploads[0].0);
    assert_eq!(text(&object["attachments"][0]["name"]), "hello.txt");
    assert!(object["mailboxIds"]["mbox-drafts"].is_true());
    assert!(object["keywords"]["$draft"].is_true());
    assert!(object["keywords"]["$seen"].is_true());
    assert!(text(&object["messageId"][0]).ends_with("@example.com"));
    let submissions = h.server().submissions();
    assert_eq!(submissions.len(), 1);
    assert_eq!(text(&submissions[0].1["emailId"]), "#draft");
    assert_eq!(text(&submissions[0].1["identityId"]), "ident-001");
    // The server's copy is in Sent, read and no longer a draft.
    assert_eq!(
        h.server().email_state("email-created-1"),
        Some(("mbox-sent".to_string(), true, false))
    );
    let resp = h.send(json!({"command": "query_emails", "mailbox_id": "mbox-sent"}));
    assert!(resp["ok"].is_true(), "query failed: {}", resp);
    let subjects: Vec<&str> = resp["emails"]
        .as_array()
        .expect("emails")
        .iter()
        .map(|e| text(&e["subject"]))
        .collect();
    assert_eq!(subjects, ["Sent from td-mail"]);

    // A From nobody sends as is refused before anything is uploaded.
    let other = drafts.join("td-mail-draft-9-10.eml");
    std::fs::write(
        &other,
        "From: other@example.com\nTo: bob@example.com\nSubject: Nope\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": other.to_string_lossy()}));
    assert!(!resp["ok"].is_true(), "{}", resp);
    assert_eq!(
        text(&resp["error"]),
        "no identity of this account sends as other@example.com; the server lists: test@example.com"
    );
    assert!(other.exists(), "the draft stays");
    assert_eq!(h.server().uploads().len(), 1);

    // A recipient the server refuses to send to is its refusal, named.
    std::fs::write(
        &other,
        "From: test@example.com\nTo: nobody@refuse.invalid\nSubject: Nope\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": other.to_string_lossy()}));
    assert!(!resp["ok"].is_true(), "{}", resp);
    assert_eq!(
        text(&resp["error"]),
        "API error: the server refused to send: forbiddenToSend: the recipient's domain is refused"
    );
    assert!(other.exists(), "the draft stays");
    assert!(
        !drafts
            .join(format!(
                "{}.lost",
                other.file_name().unwrap().to_string_lossy()
            ))
            .exists(),
        "a refusal the server answered keeps no record"
    );
    assert_eq!(h.server().submissions().len(), 1);
    assert_eq!(
        h.server().email_state("email-created-2"),
        None,
        "the copy the server made of the refused message went"
    );

    // A message sent but left a draft by the server says so, and is
    // still retired: it went.
    std::fs::write(
        &other,
        "From: test@example.com\nTo: bob@example.com\nSubject: [stay] here\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": other.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{}", resp);
    assert_eq!(
        text(&resp["kept_in"]),
        "Drafts, still a draft: invalidPatch: kept as a draft"
    );
    assert_eq!(
        h.server().email_state(text(&resp["email_id"])),
        Some(("mbox-drafts".to_string(), true, true))
    );
    assert!(!other.exists(), "retired all the same");

    // A send whose answer is lost is not sent twice: the next send asks
    // the server, finds the message and its submission, and answers as
    // the lost reply would have, creating and sending nothing.
    let lost = drafts.join("td-mail-draft-9-11.eml");
    std::fs::write(
        &lost,
        "From: test@example.com\nTo: bob@example.com\nSubject: [lose] gone\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": lost.to_string_lossy()}));
    assert!(!resp["ok"].is_true(), "{}", resp);
    let error = text(&resp["error"]).to_string();
    assert!(
        error.starts_with("HTTP error: ")
            && error
                .contains("the answer was lost, so the next send of this draft asks the server"),
        "{error}"
    );
    assert!(lost.exists(), "the draft stays");
    let record = drafts.join("td-mail-draft-9-11.eml.lost");
    assert!(record.exists(), "the attempt is recorded beside the draft");
    let created = h.server().created_emails();
    let lost_id = created.last().map(|(id, _)| id.clone()).expect("created");
    let sent_before = h.server().submissions().len();
    assert_eq!(
        h.server().email_state(&lost_id),
        Some(("mbox-sent".to_string(), true, false)),
        "the server sent it and filed it"
    );
    h.server().pad_submission_page();
    // td-mail ended and started again still asks before sending.
    h.restart();
    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);
    for field in ["messageId", "keywords", "mailboxIds"] {
        h.server().omit_filing_field(Some(field));
        let refused = h.send(json!({"command": "send_draft", "path": lost.to_string_lossy()}));
        assert!(!refused["ok"].is_true(), "{refused}");
        assert!(
            text(&refused["error"]).contains("outcome remains unknown"),
            "{refused}"
        );
        assert!(
            lost.exists() && record.exists(),
            "uncertainty survives {field}"
        );
        assert_eq!(h.server().submissions().len(), sent_before, "no retry");
        assert_eq!(
            h.server().created_emails().len(),
            created.len(),
            "no replacement"
        );
    }
    h.server().omit_filing_field(None);
    let resp = h.send(json!({"command": "send_draft", "path": lost.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{}", resp);
    assert_eq!(text(&resp["email_id"]), lost_id);
    assert_eq!(text(&resp["kept_in"]), "Sent");
    assert_eq!(
        h.server().created_emails().len(),
        created.len(),
        "created once"
    );
    assert_eq!(h.server().submissions().len(), sent_before, "sent once");
    assert!(!lost.exists(), "retired");
    assert!(!record.exists(), "the record goes with the answer");

    // A record that cannot be kept (the drafts directory read-only)
    // refuses the send before anything goes.
    let locked = drafts.join("td-mail-draft-9-21.eml");
    std::fs::write(
        &locked,
        "From: test@example.com\nTo: bob@example.com\nSubject: Locked\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let sent_before = h.server().submissions().len();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&drafts, std::fs::Permissions::from_mode(0o500)).expect("lock");
        let resp = h.send(json!({"command": "send_draft", "path": locked.to_string_lossy()}));
        std::fs::set_permissions(&drafts, std::fs::Permissions::from_mode(0o700)).expect("unlock");
        assert!(!resp["ok"].is_true(), "{}", resp);
        assert!(
            text(&resp["error"]).contains("cannot keep the record of this send"),
            "{}",
            resp
        );
    }
    assert_eq!(h.server().submissions().len(), sent_before, "nothing sent");
    std::fs::remove_file(&locked).expect("draft gone");

    // Sent, but the draft could not be retired (its name is taken in
    // sent): the record stays, so after a restart the next send asks the
    // server, finds the message, and sends nothing.
    let stuck = drafts.join("td-mail-draft-9-20.eml");
    std::fs::write(
        &stuck,
        "From: test@example.com\nTo: bob@example.com\nSubject: Stuck\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let blocker = state.path().join("td-mail/sent/td-mail-draft-9-20.eml");
    std::fs::write(&blocker, "in the way").expect("blocker");
    let resp = h.send(json!({"command": "send_draft", "path": stuck.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{}", resp);
    assert!(resp["retired_to"].is_null(), "{}", resp);
    let stuck_id = text(&resp["email_id"]).to_string();
    let stuck_record = drafts.join("td-mail-draft-9-20.eml.lost");
    assert!(
        stuck.exists() && stuck_record.exists(),
        "kept until retired"
    );
    let sent_before = h.server().submissions().len();
    let created_before = h.server().created_emails().len();
    std::fs::remove_file(&blocker).expect("blocker gone");
    h.restart();
    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);
    let resp = h.send(json!({"command": "send_draft", "path": stuck.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{}", resp);
    assert_eq!(text(&resp["email_id"]), stuck_id);
    assert_eq!(h.server().submissions().len(), sent_before, "sent once");
    assert_eq!(
        h.server().created_emails().len(),
        created_before,
        "created once"
    );
    assert!(
        !stuck.exists() && !stuck_record.exists(),
        "retired, the record with it"
    );

    // Lost, and the message was addressed to this account too, so the
    // server holds two copies with the Message-ID: the one with the
    // submission is the answer, and the delivered one is not touched.
    let echo = drafts.join("td-mail-draft-9-12.eml");
    std::fs::write(
        &echo,
        "From: test@example.com\nTo: bob@example.com\nCc: test@example.com\nSubject: [lose] echo\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": echo.to_string_lossy()}));
    assert!(
        text(&resp["error"]).contains("the answer was lost"),
        "{resp}"
    );
    let created = h.server().created_emails();
    let (sent_copy, delivered) = match created.as_slice() {
        [.., (sent_copy, _), (delivered, _)] => (sent_copy.clone(), delivered.clone()),
        _ => panic!("two copies"),
    };
    assert!(delivered.starts_with("email-delivered-"), "{delivered}");
    assert_eq!(
        h.server().email_state(&delivered),
        Some(("mbox-inbox".to_string(), false, false))
    );
    let sent_before = h.server().submissions().len();
    let resp = h.send(json!({"command": "send_draft", "path": echo.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{resp}");
    assert_eq!(text(&resp["email_id"]), sent_copy);
    assert_eq!(text(&resp["kept_in"]), "Sent");
    assert_eq!(
        h.server().created_emails().len(),
        created.len(),
        "created once"
    );
    assert_eq!(h.server().submissions().len(), sent_before, "sent once");
    assert_eq!(
        h.server().email_state(&delivered),
        Some(("mbox-inbox".to_string(), false, false)),
        "the delivered copy stays"
    );
    assert!(!echo.exists(), "retired");
    assert!(
        !drafts.join("td-mail-draft-9-12.eml.lost").exists(),
        "the record went with the retire"
    );

    // Lost, on a server that keeps no record of submissions: the copy,
    // no longer a draft, says the message went, and that is the answer.
    let forgotten = drafts.join("td-mail-draft-9-13.eml");
    std::fs::write(
        &forgotten,
        "From: test@example.com\nTo: bob@example.com\nSubject: [lose] [forget] gone\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": forgotten.to_string_lossy()}));
    assert!(
        text(&resp["error"]).contains("the answer was lost"),
        "{resp}"
    );
    let created = h.server().created_emails();
    let sent_before = h.server().submissions().len();
    let draft_copy = h.server().pad_filing_page_with_draft_copy();
    let resp = h.send(json!({"command": "send_draft", "path": forgotten.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{resp}");
    assert_eq!(text(&resp["submission_id"]), "none on record");
    assert_eq!(text(&resp["kept_in"]), "Sent");
    assert_eq!(
        h.server().email_state(text(&resp["email_id"])),
        Some(("mbox-sent".to_string(), true, false))
    );
    assert_eq!(
        h.server().created_emails().len(),
        created.len(),
        "created once"
    );
    assert_eq!(h.server().submissions().len(), sent_before, "sent once");
    assert!(
        h.server().email_state(&draft_copy).is_some(),
        "the draft is not destroyed before finding the sent copy"
    );
    assert!(!forgotten.exists(), "retired");
    assert!(
        !drafts.join("td-mail-draft-9-13.eml.lost").exists(),
        "the record went with the retire"
    );

    // Lost, and the server had refused the submission: the copy it made
    // is removed and the send is made afresh, answered this time.
    std::fs::write(
        &other,
        "From: test@example.com\nTo: nobody@refuse.invalid\nSubject: [lose] refused\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": other.to_string_lossy()}));
    assert!(!resp["ok"].is_true(), "{}", resp);
    assert!(
        text(&resp["error"]).contains("the answer was lost"),
        "{}",
        resp
    );
    let unsent_id = h
        .server()
        .created_emails()
        .last()
        .map(|(id, _)| id.clone())
        .expect("created");
    assert_eq!(
        h.server().email_state(&unsent_id),
        Some(("mbox-drafts".to_string(), true, true)),
        "the copy is there, a draft, unsent"
    );
    let resp = h.send(json!({"command": "send_draft", "path": other.to_string_lossy()}));
    assert!(!resp["ok"].is_true(), "{}", resp);
    assert_eq!(
        text(&resp["error"]),
        "API error: the server refused to send: forbiddenToSend: the recipient's domain is refused"
    );
    assert_eq!(
        h.server().email_state(&unsent_id),
        None,
        "the unsent copy went"
    );
    let afresh = h
        .server()
        .created_emails()
        .last()
        .map(|(id, _)| id.clone())
        .expect("created afresh");
    assert_ne!(afresh, unsent_id);
    assert_eq!(
        h.server().email_state(&afresh),
        None,
        "the refused copy went too"
    );
    assert!(other.exists(), "the draft stays");
    assert!(
        !drafts.join("td-mail-draft-9-10.eml.lost").exists(),
        "neither the lost attempt's record nor the fresh one's stays"
    );

    // A draft outside a drafts directory is never retired, so the send
    // that went takes its record with it rather than leaving it to
    // answer every later send.
    let elsewhere = drafts.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("elsewhere");
    let loose = elsewhere.join("td-mail-draft-9-30.eml");
    let loose_record = elsewhere.join("td-mail-draft-9-30.eml.lost");
    std::fs::write(
        &loose,
        "From: test@example.com\nTo: bob@example.com\nSubject: [lose] loose\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": loose.to_string_lossy()}));
    assert!(
        text(&resp["error"]).contains("the answer was lost"),
        "{resp}"
    );
    assert!(loose_record.exists(), "the lost attempt is recorded");
    let sent_before = h.server().submissions().len();
    let resp = h.send(json!({"command": "send_draft", "path": loose.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{resp}");
    assert_eq!(h.server().submissions().len(), sent_before, "sent once");
    assert!(
        text(&resp["warning"]).starts_with("not retired: "),
        "{resp}"
    );
    assert!(loose.exists(), "not retired");
    assert!(!loose_record.exists(), "the record went with the answer");
    std::fs::write(
        &loose,
        "From: test@example.com\nTo: bob@example.com\nSubject: again\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": loose.to_string_lossy()}));
    assert!(resp["ok"].is_true(), "{resp}");
    assert_eq!(
        h.server().submissions().len(),
        sent_before + 1,
        "a later send is sent"
    );
    assert!(!loose_record.exists(), "and keeps no record");

    // A draft that is not one says which line.
    std::fs::write(
        &other,
        "To: bob@example.com\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "send_draft", "path": other.to_string_lossy()}));
    assert_eq!(text(&resp["error"]), "the draft has no From address");
    let resp = h.send(json!({"command": "send_draft"}));
    assert_eq!(text(&resp["error"]), "missing 'path' field");
}

/// Offline, a send is refused at once with the connection's reason; the
/// draft is not queued, since the person is waiting on it.
#[test]
fn test_send_draft_offline_is_refused_not_queued() {
    let mut h = CliHarness::start_with_opts("", "", true, None, None);
    let state = testing::tempdir().expect("state dir");
    let drafts = state.path().join("td-mail/drafts");
    std::fs::create_dir_all(&drafts).expect("drafts dir");
    let draft = drafts.join("td-mail-draft-1-1.eml");
    std::fs::write(
        &draft,
        "From: test@example.com\nTo: bob@example.com\nSubject: Later\n--text follows this line--\nhi\n",
    )
    .expect("draft");
    let resp = h.send(json!({"command": "connect", "account": "test"}));
    assert!(resp["ok"].is_true(), "connect failed: {}", resp);
    let resp = h.send(json!({"command": "send_draft", "path": draft.to_string_lossy()}));
    assert!(!resp["ok"].is_true(), "{}", resp);
    assert!(
        text(&resp["error"]).starts_with("cannot send (offline mode"),
        "{}",
        resp
    );
    assert!(draft.exists());
}

#[test]
fn unreadable_message_isolated_without_hiding_its_batch() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    h.server().refuse_read("email-001", "messageInterpretation");
    let response = h.send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox", "limit":50}));
    assert!(response["ok"].is_true(), "{response}");
    let emails = response["emails"].as_array().unwrap();
    let unreadable = emails
        .iter()
        .find(|e| e["id"].as_str() == Some("email-001"))
        .unwrap();
    assert!(unreadable["unreadable"].is_true());
    assert!(unreadable["subject"].is_null());
    assert!(unreadable["preview"].is_null());
    assert!(unreadable["attachments"].is_null());
    assert_eq!(unreadable["body"].as_str(), Some("(no body)"));
    assert_eq!(unreadable["thread_id"].as_str(), Some("thread-001"));
    assert!(unreadable["received_at"].as_str().is_some());
    assert_eq!(unreadable["mailbox_ids"], json!(["mbox-inbox"]));
    assert_eq!(unreadable["blob_id"].as_str(), Some("blob-raw-email-001"));
    let downloaded = h.send(json!({"command":"download_attachment",
        "blob_id":unreadable["blob_id"], "name":"unreadable.eml",
        "content_type":"message/rfc822"}));
    assert!(downloaded["ok"].is_true(), "{downloaded}");
    let bytes = std::fs::read(downloaded["path"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, b"From: sender-email-001@example.com\r\nTo: test@example.com\r\nSubject: raw message for email-001\r\n\r\nThis is the raw body of email-001 with words to tokenize.\r\n\xff\x00");
    let repeat = h.send(json!({"command":"download_attachment",
        "blob_id":unreadable["blob_id"], "name":"unreadable.eml",
        "content_type":"message/rfc822"}));
    assert!(!repeat["ok"].is_true(), "{repeat}");
    assert_eq!(
        std::fs::read(downloaded["path"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert!(emails.iter().any(|e| e["id"].as_str() == Some("email-002")));
    let calls = h.server().read_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["fetchTextBodyValues"].is_true())
            .count(),
        emails.len() + 1
    );
    assert!(calls.iter().any(|c| c["properties"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p.as_str() == Some("blobId"))));
}

#[test]
fn singleton_work_refusal_remains_an_error_after_bounded_splitting() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    h.server().refuse_read("email-001", "storageWorkLimit");
    let response = h.send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox", "limit":50}));
    assert!(!response["ok"].is_true(), "{response}");
    let calls = h.server().read_calls();
    assert!(calls.len() <= 7);
    assert_eq!(calls.last().unwrap()["ids"].as_array().unwrap().len(), 1);
    assert!(calls.iter().all(|call| call["properties"]
        .as_array()
        .unwrap()
        .iter()
        .any(|property| property.as_str() == Some("subject"))));
}

#[test]
fn aggregate_work_refusal_splits_reads_without_losing_messages() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(50);
    h.server().limit_read_batch(1);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let response = h.send(json!({"command":"get_thread", "thread_id":"large-thread"}));
    assert!(response["ok"].is_true(), "{response}");
    let emails = response["emails"].as_array().unwrap();
    assert_eq!(emails.len(), 50);
    for (index, email) in emails.iter().enumerate() {
        assert_eq!(
            email["id"].as_str(),
            Some(format!("large-{index:04}").as_str())
        );
        assert!(!email["unreadable"].is_true());
    }
    assert_eq!(h.server().read_calls().len(), 99);
}

#[test]
fn thread_read_uses_thread_members_then_message_retrieval() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let response = h.send(json!({"command":"get_thread", "thread_id":"thread-001"}));
    assert!(response["ok"].is_true(), "{response}");
    assert_eq!(response["emails"].as_array().unwrap().len(), 1);
    assert_eq!(response["emails"][0]["id"].as_str(), Some("email-001"));
}

#[test]
fn unreadable_single_thread_member_uses_only_one_body_attempt() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    h.server().refuse_read("email-001", "messageInterpretation");
    let response = h.send(json!({"command":"get_thread", "thread_id":"thread-001"}));
    assert!(response["ok"].is_true(), "{response}");
    assert!(response["emails"][0]["unreadable"].is_true());
    assert_eq!(h.server().read_calls().len(), 2);
}

#[test]
fn automatic_rules_skip_unreadable_and_retry_after_recovery() {
    let mut h = CliHarness::start_with_mail_config("rules_mailbox_regex = \".*\"");
    std::fs::write(
        h._config_dir.path().join("rules.toml"),
        r#"
[[rule]]
name = "flag-not-trusted"
[rule.match.not]
header = "From"
regex = "trusted@example.com"
[rule.actions]
flag = true
"#,
    )
    .unwrap();
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    h.server().refuse_read("email-001", "messageInterpretation");
    let first = h.send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox"}));
    assert!(first["ok"].is_true(), "{first}");
    let unreadable = first["emails"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"].as_str() == Some("email-001"))
        .unwrap();
    assert!(unreadable["unreadable"].is_true());
    assert!(!unreadable["is_flagged"].is_true());
    let plan = h.send(json!({"command":"triage_suggest", "mailbox_id":"mbox-inbox"}));
    assert!(plan["ok"].is_true(), "{plan}");
    let kept = plan["keep"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"].as_str() == Some("email-001"))
        .unwrap();
    assert!(kept["reason"].as_str().unwrap().contains("Unreadable"));
    let opened = h.send(json!({"command":"get_email", "id":"email-001"}));
    assert!(opened["ok"].is_true(), "{opened}");
    assert!(opened["unreadable"].is_true());
    h.server().clear_read_refusal();
    let second = h.send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox"}));
    assert!(second["ok"].is_true(), "{second}");
    let refreshed = h.send(json!({"command":"get_email", "id":"email-001"}));
    assert!(refreshed["ok"].is_true(), "{refreshed}");
    assert!(!refreshed["unreadable"].is_true());
    assert!(refreshed["is_flagged"].is_true(), "{refreshed}");
}

#[test]
fn thread_larger_than_256_preserves_all_members_in_bounded_batches() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(300);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let response = h.send(json!({"command":"get_thread", "thread_id":"large-thread"}));
    assert!(response["ok"].is_true(), "{response}");
    let emails = response["emails"].as_array().unwrap();
    assert_eq!(emails.len(), 300);
    for (index, email) in emails.iter().enumerate() {
        assert_eq!(
            email["id"].as_str(),
            Some(format!("large-{index:04}").as_str())
        );
    }
    let calls = h.server().read_calls();
    assert_eq!(calls.len(), 6);
    assert!(calls
        .iter()
        .all(|c| c["ids"].as_array().unwrap().len() == 50));
}

#[test]
fn mailbox_listing_counts_large_threads_in_bounded_keyword_batches() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(300);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let response = h.send(json!({
        "command":"query_emails", "mailbox_id":"mbox-inbox", "headers_only":true
    }));
    assert!(response["ok"].is_true(), "{response}");
    assert_eq!(
        response["thread_counts"]["large-thread"]["total"].as_u64(),
        Some(300)
    );
    assert_eq!(
        response["thread_counts"]["large-thread"]["unread"].as_u64(),
        Some(300)
    );
    let calls = h.server().read_calls();
    let keyword_calls: Vec<_> = calls
        .iter()
        .filter(|call| {
            call["properties"]
                .as_array()
                .is_some_and(|properties| properties.len() == 3)
        })
        .collect();
    assert_eq!(keyword_calls.len(), 2);
    assert!(keyword_calls
        .iter()
        .all(|call| call["ids"].as_array().unwrap().len() <= 256));
}

#[test]
fn stored_metadata_failure_remains_an_error() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    h.server().refuse_read("email-001", "messageInterpretation");
    h.server().refuse_stored_metadata();
    let response = h.send(json!({"command":"get_thread", "thread_id":"thread-001"}));
    assert!(!response["ok"].is_true(), "{response}");
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("storageWorkLimit"));
    assert_eq!(h.server().read_calls().len(), 2);
}

#[test]
fn offline_open_retains_cached_unreadable_metadata() {
    let cache = testing::tempdir().unwrap();
    {
        let mut h = CliHarness::start_with_opts("", "", false, Some(cache.path().into()), None);
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        h.server().refuse_read("email-001", "messageInterpretation");
        let response = h.send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox"}));
        assert!(response["ok"].is_true(), "{response}");
    }
    let mut h = CliHarness::start_with_opts("", "", true, Some(cache.path().into()), None);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let response = h.send(json!({"command":"get_email", "id":"email-001"}));
    assert!(response["ok"].is_true(), "{response}");
    assert!(response["unreadable"].is_true());
    assert_eq!(response["blob_id"].as_str(), Some("blob-raw-email-001"));
}

#[test]
fn flag_batches_obey_discovery_and_reach_every_message() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(300);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let marked = h.send(json!({"command":"mark_mailbox_read", "mailbox_id":"mbox-inbox"}));
    assert!(marked["ok"].is_true(), "{marked}");
    let calls = h.server().update_calls();
    assert!(calls.len() > 1);
    let mut ids = std::collections::HashSet::new();
    for call in calls {
        let updates = call["update"].as_object().unwrap();
        assert!(!updates.is_empty() && updates.len() <= 64, "{call}");
        for (id, patch) in updates {
            assert!(ids.insert(id.clone()), "duplicate update {id}");
            assert!(patch["keywords/$seen"].is_true());
        }
    }
    for index in 0..300 {
        let id = format!("large-{index:04}");
        assert!(ids.contains(&id));
        assert!(h.server().email_state(&id).unwrap().1);
    }
}

#[test]
fn refused_bulk_flags_remain_queued_across_restart() {
    let mut h = CliHarness::start();
    h.server().refuse_flag(Some("email-002"));
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    // An accepted local change may still be queued for remote application.
    assert!(
        h.send(json!({"command":"mark_mailbox_read", "mailbox_id":"mbox-inbox"}))["ok"].is_true()
    );
    assert!(!h.server().email_state("email-002").unwrap().1);
    assert!(h.server().email_state("email-003").unwrap().1);
    h.server().move_test_email("email-002", "mbox-archive");
    h.server().seed_large_thread(1);
    h.server().refuse_flag(None);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    // A subsequent command also fences the backend's reconnect replay.
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert!(h.server().email_state("email-002").unwrap().1);
    assert!(!h.server().email_state("large-0000").unwrap().1);
    let replay = h.server().update_calls().pop().unwrap();
    assert_eq!(replay["update"].as_object().unwrap().len(), 1);
}

#[test]
fn refused_batch_does_not_starve_later_batches_or_override_newer_flags() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(300);
    h.server().refuse_flag(Some("large-0000"));
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(
        h.send(json!({"command":"mark_mailbox_read", "mailbox_id":"mbox-inbox"}))["ok"].is_true()
    );
    for index in 1..300 {
        assert!(
            h.server()
                .email_state(&format!("large-{index:04}"))
                .unwrap()
                .1
        );
    }
    assert!(!h.server().email_state("large-0000").unwrap().1);
    h.server().refuse_flag(None);
    assert!(h.send(json!({"command":"mark_unread", "id":"large-0000"}))["ok"].is_true());
    assert!(h.send(json!({"command":"mark_unread", "id":"large-0001"}))["ok"].is_true());
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert!(!h.server().email_state("large-0000").unwrap().1);
    assert!(!h.server().email_state("large-0001").unwrap().1);
}

#[test]
fn malformed_flag_response_stops_later_batches() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(300);
    h.server().wrong_flag_account();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(
        h.send(json!({"command":"mark_mailbox_read", "mailbox_id":"mbox-inbox"}))["ok"].is_true()
    );
    assert_eq!(h.server().update_calls().len(), 1);
}

#[test]
fn replayed_rules_preserve_newer_queued_unread_choice() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    std::fs::write(
        h._config_dir.path().join("rules.toml"),
        r#"
[[rule]]
name = "read-all"
[rule.match]
header = "From"
regex = ".*"
[rule.actions]
mark_read = true
"#,
    )
    .unwrap();
    {
        let store =
            td_kv::Store::open(&h._config_dir.path().join("cache/td-mail/test.tdkv")).unwrap();
        let mut txn = store.write();
        txn.insert(
            "op_queue",
            &td_kv::Key::from_u64(1),
            br#"{"kind":"run_rules_for_mailbox","mailbox_id":"mbox-inbox"}"#,
        );
        txn.insert(
            "op_queue",
            &td_kv::Key::from_u64(2),
            br#"{"kind":"mark_unread","op_id":1,"id":"email-002"}"#,
        );
        txn.commit().unwrap();
    }
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert!(!h.server().email_state("email-002").unwrap().1);
    assert!(h.server().email_state("email-003").unwrap().1);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert!(!h.server().email_state("email-002").unwrap().1);
}

#[test]
fn replayed_rule_plan_retains_flag_after_a_successful_move() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    std::fs::write(
        h._config_dir.path().join("rules.toml"),
        r#"
[[rule]]
name = "move"
continue_processing = true
[rule.match]
header = "From"
regex = ".*"
[rule.actions]
move_to = "Archive"
[[rule]]
name = "read"
[rule.match]
header = "From"
regex = ".*"
[rule.actions]
mark_read = true
"#,
    )
    .unwrap();
    {
        let store =
            td_kv::Store::open(&h._config_dir.path().join("cache/td-mail/test.tdkv")).unwrap();
        let mut txn = store.write();
        txn.insert(
            "op_queue",
            &td_kv::Key::from_u64(1),
            br#"{"kind":"run_rules_for_mailbox","mailbox_id":"mbox-inbox"}"#,
        );
        txn.commit().unwrap();
    }
    h.server().refuse_flag(Some("email-002"));
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert_eq!(
        h.server().email_state("email-002").unwrap().0,
        "mbox-archive"
    );
    assert!(!h.server().email_state("email-002").unwrap().1);
    h.server().refuse_flag(None);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert!(h.server().email_state("email-002").unwrap().1);
}

#[test]
fn refused_moves_remain_queued_across_replay_and_restart() {
    for error in ["accountNotFound", "unknownMethod", "invalidProperties"] {
        let mut h = CliHarness::start();
        h.server().refuse_move(Some(error));
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(
            json!({"command":"move_email", "id":"email-002", "to_mailbox_id":"mbox-archive"})
        )["ok"]
            .is_true());
        assert_eq!(h.server().email_state("email-002").unwrap().0, "mbox-inbox");
        h.restart();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
        assert_eq!(h.server().email_state("email-002").unwrap().0, "mbox-inbox");
        h.server().refuse_move(None);
        h.restart();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
        assert_eq!(
            h.server().email_state("email-002").unwrap().0,
            "mbox-archive",
            "{error}"
        );
    }
}

#[test]
fn newer_successful_move_supersedes_an_older_refused_destination() {
    let mut h = CliHarness::start();
    h.server().refuse_move(Some("invalidProperties"));
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h
        .send(json!({"command":"move_email", "id":"email-002", "to_mailbox_id":"mbox-archive"}))
        ["ok"]
        .is_true());
    h.server().refuse_move(None);
    assert!(h
        .send(json!({"command":"move_email", "id":"email-002", "to_mailbox_id":"mbox-inbox"}))
        ["ok"]
        .is_true());
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert_eq!(h.server().email_state("email-002").unwrap().0, "mbox-inbox");
}

#[test]
fn refused_rule_move_holds_its_cursor_and_later_queued_work() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    {
        let store =
            td_kv::Store::open(&h._config_dir.path().join("cache/td-mail/test.tdkv")).unwrap();
        let mut txn = store.write();
        txn.insert("op_queue", &td_kv::Key::from_u64(1), br#"{"kind":"rule_actions","actions":[{"kind":"move_email","op_id":1,"id":"email-002","to_mailbox_id":"mbox-archive"}]}"#);
        txn.insert(
            "op_queue",
            &td_kv::Key::from_u64(2),
            br#"{"kind":"mark_read","op_id":2,"id":"email-003"}"#,
        );
        txn.commit().unwrap();
    }
    h.server().refuse_move(Some("accountNotFound"));
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert_eq!(h.server().email_state("email-002").unwrap().0, "mbox-inbox");
    assert!(!h.server().email_state("email-003").unwrap().1);
    h.server().refuse_move(None);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert_eq!(
        h.server().email_state("email-002").unwrap().0,
        "mbox-archive"
    );
    assert!(h.server().email_state("email-003").unwrap().1);
}

#[test]
fn explicitly_missing_message_retires_a_queued_move() {
    let mut h = CliHarness::start();
    h.server().refuse_move(Some("invalidProperties"));
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(
        json!({"command":"move_email", "id":"missing-email", "to_mailbox_id":"mbox-archive"})
    )["ok"]
        .is_true());
    h.server().refuse_move(None);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    let attempts = h.server().update_calls().len();
    assert_eq!(attempts, 2);
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    assert_eq!(h.server().update_calls().len(), attempts);
}

#[test]
fn refused_destructions_remain_queued_across_replay_and_restart() {
    for error in [
        "wrongAccount",
        "accountNotFound",
        "unknownMethod",
        "forbidden",
    ] {
        let mut h = CliHarness::start();
        h.server().refuse_destroy(Some(error));
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(json!({"command":"destroy", "ids":["email-002"]}))["ok"].is_true());
        assert!(h.server().email_state("email-002").is_some());
        h.restart();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
        assert!(h.server().email_state("email-002").is_some());
        h.server().refuse_destroy(None);
        h.restart();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
        assert!(h.server().email_state("email-002").is_none(), "{error}");
    }
}

#[test]
fn legacy_thread_deletion_replay_obeys_set_limit_and_retires_intent() {
    let mut h = CliHarness::start();
    h.server().seed_large_thread(300);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    {
        let store =
            td_kv::Store::open(&h._config_dir.path().join("cache/td-mail/test.tdkv")).unwrap();
        let mut txn = store.write();
        txn.insert(
            "op_queue",
            &td_kv::Key::from_u64(1),
            br#"{"kind":"destroy_thread","op_id":1,"thread_id":"large-thread"}"#,
        );
        txn.commit().unwrap();
    }
    h.restart();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
    let calls = h.server().destroy_calls();
    assert_eq!(calls.len(), 5);
    let counts: Vec<_> = calls
        .iter()
        .map(|call| call["destroy"].as_array().unwrap().len())
        .collect();
    assert_eq!(counts, [64, 64, 64, 64, 44]);
    for index in 0..300 {
        assert!(h
            .server()
            .email_state(&format!("large-{index:04}"))
            .is_none());
    }
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    let store = td_kv::Store::open(&h._config_dir.path().join("cache/td-mail/test.tdkv")).unwrap();
    assert!(store
        .read()
        .table("op_queue")
        .unwrap()
        .get(&td_kv::Key::from_u64(1))
        .is_none());
}

#[test]
fn test_send_prepares_name_only_folders_and_preserves_refused_drafts() {
    let named = |id: &str, name: &str| json!({"id":id,"name":name,"role":null,"parentId":null,"totalEmails":0,"unreadEmails":0,"sortOrder":0});
    for (folders, refuse, expected_mailbox, expected_name, creates) in [
        (
            vec![named("plain-drafts", "Drafts"), named("plain-sent", "sEnT")],
            false,
            Some("plain-sent"),
            "sEnT",
            0,
        ),
        (
            vec![named("plain-drafts", "Drafts")],
            false,
            Some("plain-drafts"),
            "Drafts",
            0,
        ),
        (
            vec![named("plain-sent", "Sent")],
            false,
            Some("plain-sent"),
            "Sent",
            0,
        ),
        (
            vec![
                named("one", "Drafts"),
                named("two", "drafts"),
                json!({"id":"role-sent","name":"Role wins","role":"sent","parentId":null,"totalEmails":0,"unreadEmails":0,"sortOrder":0}),
                named("plain-sent", "Sent"),
            ],
            false,
            Some("role-sent"),
            "Role wins",
            0,
        ),
        (
            vec![
                named("plain-drafts", "Drafts"),
                named("one", "Sent"),
                named("two", "sent"),
            ],
            false,
            Some("plain-drafts"),
            "Drafts",
            0,
        ),
        (vec![], false, Some("mbox-created-1"), "Sent", 1),
        (vec![], true, None, "", 0),
        (
            vec![named("one", "Sent"), named("two", "sent")],
            false,
            None,
            "",
            0,
        ),
    ] {
        let mut h = CliHarness::start();
        h._server.sending_folders(folders, refuse);
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        let dir = testing::tempdir().unwrap();
        let draft = dir.path().join("message.eml");
        std::fs::write(&draft, "From: test@example.com\nTo: bob@example.com\nSubject: Folder preparation\n--text follows this line--\n\nHello.\n").unwrap();
        let response = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        if let Some(mailbox) = expected_mailbox {
            assert!(response["ok"].is_true(), "{response}");
            assert_eq!(response["kept_in"].as_str(), Some(expected_name));
            assert_eq!(
                h._server.email_state("email-created-1"),
                Some((mailbox.into(), true, false))
            );
            assert_eq!(h._server.submissions().len(), 1);
        } else {
            assert!(!response["ok"].is_true(), "{response}");
            assert!(response["error"].as_str().unwrap().contains(if refuse {
                "creating Sent"
            } else {
                "more than one root mailbox"
            }));
            assert!(h._server.created_emails().is_empty());
            assert!(h._server.submissions().is_empty());
            assert!(draft.exists());
        }
        assert_eq!(h._server.created_mailboxes().len(), creates);
    }
}

#[test]
fn test_lost_send_uses_submission_history_before_missing_folder_refusal() {
    for forget in [false, true] {
        let mut h = CliHarness::start();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        let dir = testing::tempdir().unwrap();
        let draft = dir.path().join("message.eml");
        let marker = if forget { "[forget]" } else { "" };
        std::fs::write(&draft, format!("From: test@example.com\nTo: bob@example.com\nSubject: [lose] {marker} Missing folders\n--text follows this line--\n\nHello.\n")).unwrap();
        let first = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert!(!first["ok"].is_true(), "{first}");
        h._server.sending_folders(vec![], false);
        let retry = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert_eq!(retry["ok"].is_true(), !forget, "{retry}");
        if forget {
            assert!(retry["error"]
                .as_str()
                .unwrap()
                .contains("recorded folder is missing"));
        }
        assert!(h._server.created_mailboxes().is_empty());
        assert_eq!(h._server.created_emails().len(), 1);
        assert!(draft.exists());
    }
}

#[test]
fn test_created_filing_folder_is_confirmed_by_returned_id() {
    for mode in [1, 2] {
        let mut h = CliHarness::start();
        h._server.sending_folders(vec![], false);
        h._server.mailbox_creation_mode(mode);
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        let dir = testing::tempdir().unwrap();
        let draft = dir.path().join("message.eml");
        std::fs::write(&draft, "From: test@example.com\nTo: bob@example.com\nSubject: Confirmation\n--text follows this line--\nHello.\n").unwrap();
        let response = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert!(!response["ok"].is_true(), "{response}");
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("not confirmed"));
        assert!(h._server.created_emails().is_empty());
        assert!(h._server.submissions().is_empty());
        assert!(draft.exists());
        assert_eq!(h._server.created_mailboxes(), vec!["Sent"]);
    }
}

#[test]
fn test_lost_send_keeps_original_folder_ids_when_names_change() {
    for refused in [false, true] {
        let mut h = CliHarness::start();
        let folder = |id: &str, name: &str| json!({"id":id,"name":name,"role":null,"parentId":null,"totalEmails":0,"unreadEmails":0,"sortOrder":0});
        h._server
            .sending_folders(vec![folder("original", "Sent")], false);
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        let dir = testing::tempdir().unwrap();
        let draft = dir.path().join("message.eml");
        let to = if refused {
            "nobody@refuse.invalid"
        } else {
            "bob@example.com"
        };
        std::fs::write(&draft, format!("From: test@example.com\nTo: {to}\nSubject: [lose] [forget] Original folders\n--text follows this line--\nHello.\n")).unwrap();
        let first = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert!(!first["ok"].is_true(), "{first}");
        h._server.sending_folders(
            vec![
                folder("original", "Renamed"),
                folder("new-drafts", "Drafts"),
                folder("new-sent", "Sent"),
            ],
            false,
        );
        let retry = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        if refused {
            assert!(
                !retry["ok"].is_true(),
                "must not report an unsent draft as sent: {retry}"
            );
            assert!(h._server.submissions().is_empty());
        } else {
            assert!(retry["ok"].is_true(), "{retry}");
            assert_eq!(retry["kept_in"].as_str(), Some("Renamed"));
            assert_eq!(h._server.created_emails().len(), 1, "must not resend");
        }
        assert!(h._server.created_mailboxes().is_empty());
    }
}

#[test]
fn test_legacy_lost_send_never_guesses_original_folders() {
    for forget in [false, true] {
        let mut h = CliHarness::start();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        let dir = testing::tempdir().unwrap();
        let draft = dir.path().join("message.eml");
        let marker = if forget { "[forget]" } else { "" };
        std::fs::write(&draft, format!("From: test@example.com\nTo: bob@example.com\nSubject: [lose] {marker} Legacy\n--text follows this line--\nHello.\n")).unwrap();
        let first = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert!(!first["ok"].is_true());
        let record = dir.path().join("message.eml.lost");
        let lines = std::fs::read_to_string(&record).unwrap();
        let legacy = lines
            .lines()
            .filter(|line| !line.starts_with("create-in ") && !line.starts_with("sent "))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&record, legacy).unwrap();
        let retry = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert_eq!(retry["ok"].is_true(), !forget, "{retry}");
        if forget {
            let error = retry["error"].as_str().unwrap();
            assert!(error.contains("no recorded filing folders"));
            assert!(error.contains(record.to_str().unwrap()));
            assert!(record.exists());
        }
        assert_eq!(h._server.created_emails().len(), 1);
    }
}

#[test]
fn test_new_drafts_folder_cannot_make_unsent_copy_look_sent() {
    let mut h = CliHarness::start();
    let folder = |id: &str, name: &str| json!({"id":id,"name":name,"role":null,"parentId":null,"totalEmails":0,"unreadEmails":0,"sortOrder":0});
    h._server
        .sending_folders(vec![folder("original", "Sent")], false);
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let dir = testing::tempdir().unwrap();
    let draft = dir.path().join("message.eml");
    std::fs::write(&draft, "From: test@example.com\nTo: nobody@refuse.invalid\nSubject: [lose] Unsent\n--text follows this line--\nHello.\n").unwrap();
    assert!(
        !h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}))["ok"].is_true()
    );
    h._server.sending_folders(
        vec![folder("original", "Sent"), folder("new-drafts", "Drafts")],
        false,
    );
    let retry = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
    assert!(
        !retry["ok"].is_true(),
        "unsent copy reported as sent: {retry}"
    );
    assert!(retry["error"].as_str().unwrap().contains("forbiddenToSend"));
    assert_eq!(h._server.email_state("email-created-1"), None);
    assert!(h._server.submissions().is_empty());
    assert!(draft.exists());
}

#[test]
fn test_unresolved_draft_outside_recorded_creation_folder_stays_untouched() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let dir = testing::tempdir().unwrap();
    let draft = dir.path().join("message.eml");
    std::fs::write(&draft, "From: test@example.com\nTo: nobody@refuse.invalid\nSubject: [lose] Moved draft\n--text follows this line--\nHello.\n").unwrap();
    assert!(
        !h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}))["ok"].is_true()
    );
    h._server.move_test_email("email-created-1", "mbox-sent");
    let retry = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
    assert!(!retry["ok"].is_true(), "{retry}");
    assert!(retry["error"]
        .as_str()
        .unwrap()
        .contains("unresolved draft outside"));
    assert_eq!(h._server.created_emails().len(), 1);
    assert_eq!(
        h._server.email_state("email-created-1"),
        Some(("mbox-sent".into(), true, true))
    );
    assert!(dir.path().join("message.eml.lost").exists());
}

#[test]
fn test_invalid_filing_id_refuses_before_sending_or_writing_a_record() {
    for id in ["", "bad\nId", "invalid/id"] {
        let mut h = CliHarness::start();
        h._server.sending_folders(vec![json!({"id":id,"name":"Sent","role":"sent","parentId":null,"totalEmails":0,"unreadEmails":0,"sortOrder":0})], false);
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        let dir = testing::tempdir().unwrap();
        let draft = dir.path().join("message.eml");
        std::fs::write(&draft, "From: test@example.com\nTo: bob@example.com\nSubject: Invalid folder\n--text follows this line--\nHello.\n").unwrap();
        let response = h.send(json!({"command":"send_draft", "path":draft.to_string_lossy()}));
        assert!(!response["ok"].is_true(), "{response}");
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("filing mailbox ID is not a JMAP Id"));
        assert!(h._server.created_emails().is_empty());
        assert!(h._server.submissions().is_empty());
        assert!(!dir.path().join("message.eml.lost").exists());
    }
}

#[test]
fn summary_listing_fetches_body_on_open_and_keeps_it_on_refresh() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    let listing =
        h.send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox", "headers_only":true}));
    assert!(listing["ok"].is_true(), "{listing}");
    let calls = h.server().read_calls();
    assert!(calls.iter().any(|call| call["properties"]
        .as_array()
        .is_some_and(|props| props.iter().any(|p| p.as_str() == Some("subject")))));
    assert!(calls
        .iter()
        .all(|call| call["fetchTextBodyValues"].as_bool() != Some(true)));
    assert!(calls.iter().all(
        |call| !call["properties"]
            .as_array()
            .is_some_and(|props| props.iter().any(|p| matches!(
                p.as_str(),
                Some("bodyValues" | "textBody" | "htmlBody" | "preview" | "attachments")
            )))
    ));
    let before = calls.len();
    let opened = h.send(json!({"command":"get_email", "id":"email-001"}));
    assert!(opened["ok"].is_true(), "{opened}");
    assert_ne!(opened["body"].as_str(), Some("(no body)"));
    assert!(h
        .server()
        .read_calls()
        .get(before..)
        .unwrap()
        .iter()
        .any(|call| call["fetchTextBodyValues"].as_bool() == Some(true)));
    assert!(h
        .send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox", "headers_only":true}))
        ["ok"]
        .is_true());
    let before = h.server().read_calls().len();
    let cached = h.send(json!({"command":"get_email", "id":"email-001"}));
    assert!(cached["ok"].is_true(), "{cached}");
    assert_eq!(cached["body"], opened["body"]);
    assert_eq!(h.server().read_calls().len(), before);
}

#[test]
fn summary_listing_only_fetches_bodies_where_automatic_rules_apply() {
    for (pattern, expect_bodies) in [("^Archive$", false), ("(?i)^inbox$", true)] {
        let mut h =
            CliHarness::start_with_mail_config(&format!("rules_mailbox_regex = \"{pattern}\""));
        std::fs::write(
            h._config_dir.path().join("rules.toml"),
            r#"
[[rule]]
name = "folder-rule"
[rule.match]
header = "Subject"
regex = "a subject phrase that is absent"
[rule.actions]
flag = true
"#,
        )
        .unwrap();
        h.restart();
        assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
        assert!(h.send(json!({"command":"list_mailboxes"}))["ok"].is_true());
        let listing = h.send(json!({
            "command":"query_emails", "mailbox_id":"mbox-inbox", "headers_only":true
        }));
        assert!(listing["ok"].is_true(), "{listing}");
        let calls = h.server().read_calls();
        assert!(!calls.is_empty());
        assert_eq!(
            calls
                .iter()
                .any(|call| call["fetchTextBodyValues"].as_bool() == Some(true)),
            expect_bodies,
            "rule scope {pattern}: {calls:?}"
        );
    }
}

#[test]
fn offline_summary_cache_does_not_claim_to_contain_message_bodies() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h
        .send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox", "headers_only":true}))
        ["ok"]
        .is_true());
    h.restart_with_offline(true);
    let connected = h.send(json!({"command":"connect", "account":"test"}));
    assert!(
        connected["ok"].is_true() && connected["offline"].is_true(),
        "{connected}"
    );
    assert!(h
        .send(json!({"command":"query_emails", "mailbox_id":"mbox-inbox", "headers_only":true}))
        ["ok"]
        .is_true());
    for command in [
        json!({"command":"query_emails", "mailbox_id":"mbox-inbox"}),
        json!({"command":"triage_suggest", "mailbox_id":"mbox-inbox"}),
        json!({"command":"get_email", "id":"email-001"}),
        json!({"command":"get_thread", "thread_id":"thread-001"}),
    ] {
        let response = h.send(command);
        assert!(!response["ok"].is_true(), "{response}");
        assert!(
            response["error"].as_str().unwrap().contains("cached"),
            "{response}"
        );
    }
}

#[test]
fn request_logs_report_timings_without_message_payloads() {
    let mut h = CliHarness::start();
    assert!(h.send(json!({"command":"connect", "account":"test"}))["ok"].is_true());
    assert!(h.send(json!({"command":"get_email", "id":"email-001"}))["ok"].is_true());
    let log =
        std::fs::read_to_string(h._config_dir.path().join("state/td-mail/td-mail.log")).unwrap();
    assert!(
        log.contains("HTTP methods=Email/get attempt=1 elapsed_ms="),
        "{log}"
    );
    assert!(log.contains("status=200 request_bytes="), "{log}");
    assert!(log.contains("attempts=1 retry_wait_ms=0"), "{log}");
    assert!(!log.contains("Request body:"));
    assert!(!log.contains("Response body ("));
}

#[test]
fn endpoint_export_needs_neither_password_execution_nor_a_running_replica() {
    let root = td_fs::TempDir::new("mail-endpoint-export").unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        r#"[account.replica]
well_known_url = "http://localhost/.well-known/jmap"
username = "mail@example.com"
password_command = "this-command-must-not-run"
unix_socket = "/absent-replica/socket"
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_td-mail"))
        .arg("--fetch-unix-endpoints")
        .arg(format!("--config={}", config.display()))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"td-mail-unix-v1\n/absent-replica/socket\n");
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("cache").exists());
}
