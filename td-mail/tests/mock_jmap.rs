use crate::json::Json as Value;
use crate::json::ObjectBuilder;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone)]
struct EmailRecord {
    id: String,
    thread_id: String,
    from_name: Option<String>,
    from_email: String,
    subject: String,
    body: String,
    received_at: String,
    mailbox_id: String,
    is_read: bool,
    attachments: Vec<Value>,
}

impl EmailRecord {
    fn to_jmap_email(&self) -> Value {
        let mut keywords = ObjectBuilder::new();
        if self.is_read {
            keywords.insert("$seen".to_string(), json!(true));
        }
        let keywords = keywords.build();

        json!({
            "id": self.id,
            "blobId": format!("blob-raw-{}", self.id),
            "threadId": self.thread_id,
            "from": [{"name": self.from_name, "email": self.from_email}],
            "to": [{"name": "Test User", "email": "test@example.com"}],
            "cc": null,
            "replyTo": null,
            "subject": self.subject,
            "receivedAt": self.received_at,
            "sentAt": self.received_at,
            "preview": self.body.chars().take(100).collect::<String>(),
            "textBody": [{"partId": "1"}],
            "bodyValues": {"1": {"value": self.body, "isEncodingProblem": false, "isTruncated": false}},
            "keywords": keywords,
            "mailboxIds": {self.mailbox_id.clone(): true},
            "messageId": [format!("<{}@example.com>", self.id)],
            "references": null,
            "attachments": self.attachments.clone()
        })
    }
}

struct MockState {
    emails: HashMap<String, EmailRecord>,
}

impl MockState {
    fn new() -> Self {
        let mut emails = HashMap::new();

        let seed = vec![
            EmailRecord {
                id: "email-001".to_string(),
                thread_id: "thread-001".to_string(),
                from_name: Some("Alice".to_string()),
                from_email: "alice@example.com".to_string(),
                subject: "Hello World".to_string(),
                body: "This is the body of email 001.".to_string(),
                received_at: "2025-01-15T10:30:00Z".to_string(),
                mailbox_id: "mbox-inbox".to_string(),
                is_read: true,
                attachments: vec![json!({
                    "partId": "2",
                    "blobId": "blob-att-001",
                    "type": "application/pdf",
                    "name": "test-document.pdf",
                    "size": 1024
                })],
            },
            EmailRecord {
                id: "email-002".to_string(),
                thread_id: "thread-002".to_string(),
                from_name: Some("Bob".to_string()),
                from_email: "bob@example.com".to_string(),
                subject: "Meeting Tomorrow".to_string(),
                body: "Let's meet at 10am.".to_string(),
                received_at: "2025-12-06T11:00:00Z".to_string(),
                mailbox_id: "mbox-inbox".to_string(),
                is_read: false,
                attachments: vec![],
            },
            EmailRecord {
                id: "email-003".to_string(),
                thread_id: "thread-003".to_string(),
                from_name: Some("Loan Team".to_string()),
                from_email: "loan@equityexcelloans.com".to_string(),
                subject: "HARP program is approaching cap".to_string(),
                body: "Spammy mortgage email.".to_string(),
                received_at: "2025-12-20T09:00:00Z".to_string(),
                mailbox_id: "mbox-inbox".to_string(),
                is_read: false,
                attachments: vec![],
            },
            EmailRecord {
                id: "email-004".to_string(),
                thread_id: "thread-004".to_string(),
                from_name: Some("Delta".to_string()),
                from_email: "DeltaAirLines@t.delta.com".to_string(),
                subject: "Your Flight Receipt".to_string(),
                body: "Flight receipt details.".to_string(),
                received_at: "2025-12-22T08:00:00Z".to_string(),
                mailbox_id: "mbox-inbox".to_string(),
                is_read: true,
                attachments: vec![],
            },
            EmailRecord {
                id: "email-005".to_string(),
                thread_id: "thread-005".to_string(),
                from_name: Some("Archive Seed".to_string()),
                from_email: "seed@example.com".to_string(),
                subject: "Already archived".to_string(),
                body: "Archive mailbox seed".to_string(),
                received_at: "2025-11-01T08:00:00Z".to_string(),
                mailbox_id: "mbox-archive".to_string(),
                is_read: true,
                attachments: vec![],
            },
        ];

        for e in seed {
            emails.insert(e.id.clone(), e);
        }

        Self { emails }
    }

    fn query_email_ids(&self, filter: &Value, limit: usize, position: usize) -> Vec<String> {
        let mut in_mailbox: Option<String> = None;
        let mut text: Option<String> = None;
        let mut after: Option<String> = None;
        let mut before: Option<String> = None;

        fn parse_filter(
            f: &Value,
            in_mailbox: &mut Option<String>,
            text: &mut Option<String>,
            after: &mut Option<String>,
            before: &mut Option<String>,
        ) {
            if f.as_object().is_some() {
                if let Some(v) = f.get("inMailbox").and_then(|v| v.as_str()) {
                    *in_mailbox = Some(v.to_string());
                }
                if let Some(v) = f.get("text").and_then(|v| v.as_str()) {
                    *text = Some(v.to_ascii_lowercase());
                }
                if let Some(v) = f.get("after").and_then(|v| v.as_str()) {
                    *after = Some(v.to_string());
                }
                if let Some(v) = f.get("before").and_then(|v| v.as_str()) {
                    *before = Some(v.to_string());
                }
                if f.get("operator").and_then(|v| v.as_str()) == Some("AND") {
                    if let Some(conditions) = f.get("conditions").and_then(|v| v.as_array()) {
                        for c in conditions {
                            parse_filter(c, in_mailbox, text, after, before);
                        }
                    }
                }
            }
        }

        parse_filter(filter, &mut in_mailbox, &mut text, &mut after, &mut before);

        let mut emails: Vec<&EmailRecord> = self
            .emails
            .values()
            .filter(|e| {
                if let Some(ref mbox) = in_mailbox {
                    if &e.mailbox_id != mbox {
                        return false;
                    }
                }
                if let Some(ref q) = text {
                    let hay = format!(
                        "{} {} {}",
                        e.subject.to_ascii_lowercase(),
                        e.body.to_ascii_lowercase(),
                        e.from_email.to_ascii_lowercase()
                    );
                    if !hay.contains(q) {
                        return false;
                    }
                }
                if let Some(ref a) = after {
                    if e.received_at <= *a {
                        return false;
                    }
                }
                if let Some(ref b) = before {
                    if e.received_at >= *b {
                        return false;
                    }
                }
                true
            })
            .collect();

        emails.sort_by(|a, b| b.received_at.cmp(&a.received_at));

        emails
            .into_iter()
            .skip(position)
            .take(limit)
            .map(|e| e.id.clone())
            .collect()
    }

    fn apply_email_set(&mut self, args: &Value) -> Value {
        let mut updated = ObjectBuilder::new();
        let mut not_updated = ObjectBuilder::new();
        let mut not_destroyed = ObjectBuilder::new();

        if let Some(update) = args.get("update").and_then(Value::as_object) {
            for (id, patch) in update {
                let Some(email) = self.emails.get_mut(id) else {
                    not_updated.insert(id.clone(), json!({"type": "notFound"}));
                    continue;
                };

                if let Some(seen) = patch.get("keywords/$seen") {
                    email.is_read = seen.as_bool().unwrap_or(false);
                }
                if let Some(mailbox_ids) = patch.get("mailboxIds").and_then(Value::as_object) {
                    if let Some((target, _)) = mailbox_ids
                        .iter()
                        .find(|(_, v)| v.as_bool().unwrap_or(false))
                    {
                        email.mailbox_id = target.clone();
                    }
                }
                updated.insert(id.clone(), Value::Null);
            }
        }

        if let Some(destroy) = args.get("destroy").and_then(Value::as_array) {
            for idv in destroy {
                let Some(id) = idv.as_str() else {
                    continue;
                };
                if self.emails.remove(id).is_none() {
                    not_destroyed.insert(id.to_string(), json!({"type": "notFound"}));
                }
            }
        }

        let mut resp = ObjectBuilder::new()
            .set("accountId", "account-001")
            .set("oldState", "estate-001")
            .set("newState", "estate-002");
        let updated = updated.build();
        let not_updated = not_updated.build();
        let not_destroyed = not_destroyed.build();
        if !updated.as_object().unwrap_or(&[]).is_empty() {
            resp = resp.set("updated", updated);
        }
        if !not_updated.as_object().unwrap_or(&[]).is_empty() {
            resp = resp.set("notUpdated", not_updated);
        }
        if !not_destroyed.as_object().unwrap_or(&[]).is_empty() {
            resp = resp.set("notDestroyed", not_destroyed);
        }
        resp.build()
    }
}

pub struct MockJmapServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockJmapServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let state = Arc::new(Mutex::new(MockState::new()));

        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on listener");

        let handle = thread::spawn(move || {
            Self::serve(listener, shutdown_clone, state, port);
        });

        MockJmapServer {
            port,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn serve(
        listener: TcpListener,
        shutdown: Arc<AtomicBool>,
        state: Arc<Mutex<MockState>>,
        port: u16,
    ) {
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("set blocking on stream");
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .ok();
                    Self::handle_connection(stream, port, &state);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    fn handle_connection(
        mut stream: std::net::TcpStream,
        port: u16,
        state: &Arc<Mutex<MockState>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }

        let mut content_length: usize = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() {
                return;
            }
            let trimmed = header.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some(val) = trimmed.strip_prefix("Content-Length:") {
                if let Ok(len) = val.trim().parse() {
                    content_length = len;
                }
            }
            if let Some(val) = trimmed.strip_prefix("content-length:") {
                if let Ok(len) = val.trim().parse() {
                    content_length = len;
                }
            }
        }

        let body = if content_length > 0 {
            let mut buf = vec![0u8; content_length];
            if reader.read_exact(&mut buf).is_err() {
                return;
            }
            String::from_utf8_lossy(&buf).to_string()
        } else {
            String::new()
        };

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0];
        let path = parts[1];

        let (status, response_body, content_type) =
            if method == "GET" && path.contains("/.well-known/jmap") {
                let (s, b) = Self::handle_session(port);
                (s, b, "application/json")
            } else if method == "POST" && path.contains("/api") {
                let (s, b) = Self::handle_api(&body, state);
                (s, b, "application/json")
            } else if method == "GET" && path.starts_with("/download/") {
                let (s, b) = Self::handle_download(path);
                (s, b, "application/octet-stream")
            } else {
                (
                    "404 Not Found".to_string(),
                    json!({"error": "not found"}).to_string(),
                    "application/json",
                )
            };

        let response = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            content_type,
            response_body.len(),
            response_body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    fn handle_download(path: &str) -> (String, String) {
        // Path format: /download/{accountId}/{blobId}/{name}?type={type}
        let path_no_query = path.split('?').next().unwrap_or(path);
        let segments: Vec<&str> = path_no_query.split('/').collect();
        // segments: ["", "download", accountId, blobId, name]
        let blob_id = segments.get(3).copied().unwrap_or("");
        if blob_id == "blob-att-001" {
            let fake_pdf_content = b"%PDF-1.4 fake test content for blob-att-001";
            (
                "200 OK".to_string(),
                String::from_utf8_lossy(fake_pdf_content).to_string(),
            )
        } else if let Some(email_id) = blob_id.strip_prefix("blob-raw-") {
            // Synthesize a raw RFC822 message for the spam classifier to tokenize.
            let raw = format!(
                "From: sender-{id}@example.com\r\n\
                 To: test@example.com\r\n\
                 Subject: raw message for {id}\r\n\
                 \r\n\
                 This is the raw body of {id} with words to tokenize.\r\n",
                id = email_id
            );
            ("200 OK".to_string(), raw)
        } else {
            ("404 Not Found".to_string(), "blob not found".to_string())
        }
    }

    fn handle_session(port: u16) -> (String, String) {
        let session = json!({
            "username": "test@example.com",
            "apiUrl": format!("http://127.0.0.1:{}/api", port),
            "downloadUrl": format!("http://127.0.0.1:{}/download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}", port),
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "account-001"
            },
            "accounts": {
                "account-001": {
                    "name": "Test Account",
                    "isPersonal": true,
                    "isReadOnly": false
                }
            }
        });
        ("200 OK".to_string(), session.to_string())
    }

    fn handle_api(body: &str, state: &Arc<Mutex<MockState>>) -> (String, String) {
        let request = match crate::json::parse(body) {
            Ok(v) => v,
            Err(_) => {
                return (
                    "400 Bad Request".to_string(),
                    json!({"error": "invalid JSON"}).to_string(),
                );
            }
        };

        let method_calls = match request.get("methodCalls").and_then(|v| v.as_array()) {
            Some(calls) => calls,
            None => {
                return (
                    "400 Bad Request".to_string(),
                    json!({"error": "missing methodCalls"}).to_string(),
                );
            }
        };

        let mut responses = Vec::new();

        for call in method_calls {
            let arr = match call.as_array() {
                Some(a) if a.len() >= 3 => a,
                _ => continue,
            };
            let method_name = arr[0].as_str().unwrap_or("");
            let args = &arr[1];
            let call_id = arr[2].as_str().unwrap_or("0");

            let response = match method_name {
                "Mailbox/get" => json!([
                    "Mailbox/get",
                    {
                        "accountId": "account-001",
                        "state": "state-001",
                        "list": [
                            {
                                "id": "mbox-inbox",
                                "name": "INBOX",
                                "role": "inbox",
                                "totalEmails": 4,
                                "unreadEmails": 2,
                                "sortOrder": 1
                            },
                            {
                                "id": "mbox-archive",
                                "name": "Archive",
                                "role": "archive",
                                "totalEmails": 1,
                                "unreadEmails": 0,
                                "sortOrder": 3
                            },
                            {
                                "id": "mbox-trash",
                                "name": "Trash",
                                "role": "trash",
                                "totalEmails": 0,
                                "unreadEmails": 0,
                                "sortOrder": 4
                            }
                        ],
                        "notFound": []
                    },
                    call_id
                ]),
                "Email/query" => {
                    let filter = args.get("filter").cloned().unwrap_or_else(|| json!({}));
                    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
                    let position =
                        args.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let ids = {
                        let guard = state.lock().expect("state lock");
                        guard.query_email_ids(&filter, limit, position)
                    };
                    json!([
                        "Email/query",
                        {
                            "accountId": "account-001",
                            "queryState": "qstate-001",
                            "ids": ids,
                            "position": position,
                            "total": null
                        },
                        call_id
                    ])
                }
                "Email/get" => {
                    let requested_ids = args
                        .get("ids")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                        .unwrap_or_default();

                    let list = {
                        let guard = state.lock().expect("state lock");
                        if requested_ids.is_empty() {
                            guard
                                .emails
                                .values()
                                .map(EmailRecord::to_jmap_email)
                                .collect::<Vec<_>>()
                        } else {
                            requested_ids
                                .into_iter()
                                .filter_map(|id| {
                                    guard.emails.get(id).map(EmailRecord::to_jmap_email)
                                })
                                .collect::<Vec<_>>()
                        }
                    };

                    json!([
                        "Email/get",
                        {
                            "accountId": "account-001",
                            "state": "estate-001",
                            "list": list,
                            "notFound": []
                        },
                        call_id
                    ])
                }
                "Email/set" => {
                    let payload = {
                        let mut guard = state.lock().expect("state lock");
                        guard.apply_email_set(args)
                    };
                    json!(["Email/set", payload, call_id])
                }
                "Thread/get" => json!([
                    "Thread/get",
                    {
                        "accountId": "account-001",
                        "state": "tstate-001",
                        "list": [
                            {
                                "id": "thread-001",
                                "emailIds": ["email-001"]
                            }
                        ],
                        "notFound": []
                    },
                    call_id
                ]),
                _ => json!([
                    "error",
                    {
                        "type": "unknownMethod",
                        "description": format!("Unknown method: {}", method_name)
                    },
                    call_id
                ]),
            };
            responses.push(response);
        }

        let jmap_response = json!({
            "methodResponses": responses,
            "sessionState": "session-001"
        });

        ("200 OK".to_string(), jmap_response.to_string())
    }
}

impl Drop for MockJmapServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
