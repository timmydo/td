use crate::json::Json as Value;
use crate::json::ObjectBuilder;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Now as JMAP's `UTCDate`, so what the mock makes is inside the window
/// a client asks about.
fn now_rfc3339() -> String {
    let c = crate::civil::unix_to_civil_utc(crate::civil::now_unix());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

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
    is_draft: bool,
    attachments: Vec<Value>,
    /// The Message-ID an `Email/set` create gave the message; the seed
    /// messages make theirs up from their ids.
    message_id: Option<String>,
}

impl EmailRecord {
    fn to_jmap_email(&self) -> Value {
        let mut keywords = ObjectBuilder::new();
        if self.is_read {
            keywords.insert("$seen".to_string(), json!(true));
        }
        if self.is_draft {
            keywords.insert("$draft".to_string(), json!(true));
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
            "messageId": [self
                .message_id
                .clone()
                .unwrap_or_else(|| format!("<{}@example.com>", self.id))],
            "references": null,
            "attachments": self.attachments.clone()
        })
    }
}

struct MockState {
    emails: HashMap<String, EmailRecord>,
    /// Every blob uploaded: its id, type and bytes.
    uploads: Vec<(String, String, Vec<u8>)>,
    /// Every `Email/set` create object, with the id it was given.
    created: Vec<(String, Value)>,
    /// Every `EmailSubmission/set` create object, with the id it was given.
    submissions: Vec<(String, Value)>,
    /// Each submission's id with the id of the message it sent, resolved,
    /// and when it was sent, as the server lists it.
    submission_emails: Vec<(String, String, String)>,
    /// Creation ids of this connection's request, for `#ref` resolution.
    creation_refs: HashMap<String, String>,
    /// Subjects marked `[lose]` whose first create was answered with a
    /// broken reply, the request served all the same: once each.
    lost: Vec<String>,
    /// The reply to the request being served is to be lost.
    lose_answer: bool,
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
                is_draft: false,
                attachments: vec![json!({
                    "partId": "2",
                    "blobId": "blob-att-001",
                    "type": "application/pdf",
                    "name": "test-document.pdf",
                    "size": 1024
                })],
                message_id: None,
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
                is_draft: false,
                attachments: vec![],
                message_id: None,
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
                is_draft: false,
                attachments: vec![],
                message_id: None,
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
                is_draft: false,
                attachments: vec![],
                message_id: None,
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
                is_draft: false,
                attachments: vec![],
                message_id: None,
            },
        ];

        for e in seed {
            emails.insert(e.id.clone(), e);
        }

        Self {
            emails,
            uploads: Vec::new(),
            created: Vec::new(),
            submissions: Vec::new(),
            submission_emails: Vec::new(),
            creation_refs: HashMap::new(),
            lost: Vec::new(),
            lose_answer: false,
        }
    }

    /// An `Email/set` create: the object becomes a record in the first
    /// mailbox it names, its text the named body value, its attachments
    /// as given; the id is `email-created-N`.
    fn create_email(&mut self, creation_id: &str, object: &Value) -> Value {
        let id = format!("email-created-{}", self.created.len() + 1);
        let from = object
            .get("from")
            .and_then(Value::as_array)
            .and_then(|f| f.first());
        let part_id = object
            .get_path(&["textBody"])
            .and_then(Value::as_array)
            .and_then(|t| t.first())
            .and_then(|t| t.get("partId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let body = object
            .get_path(&["bodyValues", part_id, "value"])
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mailbox_id = object
            .get("mailboxIds")
            .and_then(Value::as_object)
            .and_then(|m| m.iter().find(|(_, v)| v.as_bool().unwrap_or(false)))
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        let attachments = object
            .get("attachments")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let blob_id = a.get("blobId").and_then(Value::as_str).unwrap_or("");
                        let size = self
                            .uploads
                            .iter()
                            .find(|(id, _, _)| id == blob_id)
                            .map_or(0, |(_, _, bytes)| bytes.len());
                        json!({
                            "partId": format!("{}", i + 2),
                            "blobId": blob_id,
                            "type": a.get("type").cloned().unwrap_or(Value::Null),
                            "name": a.get("name").cloned().unwrap_or(Value::Null),
                            "size": size
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let record = EmailRecord {
            id: id.clone(),
            thread_id: format!("thread-{id}"),
            from_name: from
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string),
            from_email: from
                .and_then(|f| f.get("email"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            subject: object
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            body,
            received_at: now_rfc3339(),
            mailbox_id,
            is_read: object
                .get_path(&["keywords", "$seen"])
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_draft: object
                .get_path(&["keywords", "$draft"])
                .and_then(Value::as_bool)
                .unwrap_or(false),
            attachments,
            message_id: object
                .get("messageId")
                .and_then(Value::as_array)
                .and_then(|ids| ids.first())
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        // A subject marked [lose] has its first create's answer lost on
        // the way back, the server having done everything asked.
        let subject = object.get("subject").and_then(Value::as_str).unwrap_or("");
        if subject.contains("[lose]") && !self.lost.iter().any(|s| s == subject) {
            self.lost.push(subject.to_string());
            self.lose_answer = true;
        }
        self.emails.insert(id.clone(), record);
        self.creation_refs
            .insert(creation_id.to_string(), id.clone());
        self.created.push((id.clone(), object.clone()));
        json!({
            "id": id,
            "blobId": format!("blob-raw-{id}"),
            "threadId": format!("thread-{id}"),
            "size": 1
        })
    }

    /// A patch as `Email/set` update and `onSuccessUpdateEmail` write it:
    /// `keywords/$seen`, `keywords/$draft`, `mailboxIds/ID` and a whole
    /// `mailboxIds`.
    fn patch_email(email: &mut EmailRecord, patch: &Value) {
        if let Some(seen) = patch.get("keywords/$seen") {
            email.is_read = seen.as_bool().unwrap_or(false);
        }
        if let Some(draft) = patch.get("keywords/$draft") {
            email.is_draft = draft.as_bool().unwrap_or(false);
        }
        if let Some(mailbox_ids) = patch.get("mailboxIds").and_then(Value::as_object) {
            if let Some((target, _)) = mailbox_ids
                .iter()
                .find(|(_, v)| v.as_bool().unwrap_or(false))
            {
                email.mailbox_id = target.clone();
            }
        }
        if let Some(pairs) = patch.as_object() {
            for (key, value) in pairs {
                if let Some(id) = key.strip_prefix("mailboxIds/") {
                    if value.as_bool().unwrap_or(false) {
                        email.mailbox_id = id.to_string();
                    }
                }
            }
        }
    }

    /// `EmailSubmission/set`: one create, its `emailId` a `#ref` or an id,
    /// its `identityId` the one identity; a recipient at `refuse.invalid`
    /// is refused as a server would refuse a forbidden one. On success
    /// the `onSuccessUpdateEmail` patches are applied and answered as an
    /// implicit `Email/set`.
    fn apply_submission_set(&mut self, args: &Value, call_id: &str) -> Vec<Value> {
        let mut created = ObjectBuilder::new();
        let mut not_created = ObjectBuilder::new();
        let mut succeeded = Vec::new();
        if let Some(create) = args.get("create").and_then(Value::as_object) {
            for (creation_id, object) in create {
                let email_ref = object.get("emailId").and_then(Value::as_str).unwrap_or("");
                let email_id = match email_ref.strip_prefix('#') {
                    Some(reference) => self.creation_refs.get(reference).cloned(),
                    None => Some(email_ref.to_string()),
                };
                let Some(email_id) = email_id.filter(|id| self.emails.contains_key(id)) else {
                    not_created.insert(
                        creation_id.clone(),
                        json!({"type": "invalidProperties", "properties": ["emailId"]}),
                    );
                    continue;
                };
                if object.get("identityId").and_then(Value::as_str) != Some("ident-001") {
                    not_created.insert(
                        creation_id.clone(),
                        json!({
                            "type": "invalidProperties",
                            "properties": ["identityId"],
                            "description": "unknown identity"
                        }),
                    );
                    continue;
                }
                let refused = self
                    .created
                    .iter()
                    .find(|(id, _)| *id == email_id)
                    .is_some_and(|(_, object)| {
                        object
                            .get("to")
                            .and_then(Value::as_array)
                            .is_some_and(|to| {
                                to.iter().any(|a| {
                                    a.get("email")
                                        .and_then(Value::as_str)
                                        .is_some_and(|e| e.ends_with("@refuse.invalid"))
                                })
                            })
                    });
                if refused {
                    not_created.insert(
                        creation_id.clone(),
                        json!({
                            "type": "forbiddenToSend",
                            "description": "the recipient's domain is refused"
                        }),
                    );
                    continue;
                }
                let id = format!("sub-{}", self.submissions.len() + 1);
                self.submissions.push((id.clone(), object.clone()));
                let create_object = self
                    .created
                    .iter()
                    .find(|(created_id, _)| *created_id == email_id)
                    .map(|(_, object)| object.clone());
                let subject = self
                    .emails
                    .get(&email_id)
                    .map(|e| e.subject.clone())
                    .unwrap_or_default();
                // A subject marked [forget] is sent by a server that keeps
                // no record of the submission afterwards (RFC 8621 §7
                // lets it delete them): created and answered, never
                // listed again.
                let send_at = now_rfc3339();
                if !subject.contains("[forget]") {
                    self.submission_emails
                        .push((id.clone(), email_id.clone(), send_at.clone()));
                }
                // A message addressed to this account is delivered to it
                // too: a copy in INBOX with the same Message-ID, as a
                // server would make.
                let to_self = create_object.as_ref().is_some_and(|object| {
                    ["to", "cc", "bcc"].iter().any(|field| {
                        object
                            .get(*field)
                            .and_then(Value::as_array)
                            .is_some_and(|list| {
                                list.iter().any(|a| {
                                    a.get("email").and_then(Value::as_str)
                                        == Some("test@example.com")
                                })
                            })
                    })
                });
                if let (true, Some(object), Some(copy)) =
                    (to_self, create_object, self.emails.get(&email_id).cloned())
                {
                    let delivered_id = format!("email-delivered-{}", self.created.len() + 1);
                    self.emails.insert(
                        delivered_id.clone(),
                        EmailRecord {
                            id: delivered_id.clone(),
                            mailbox_id: "mbox-inbox".to_string(),
                            is_read: false,
                            is_draft: false,
                            ..copy
                        },
                    );
                    self.created.push((delivered_id, object));
                }
                created.insert(
                    creation_id.clone(),
                    json!({"id": id, "undoStatus": "final", "sendAt": send_at}),
                );
                succeeded.push((creation_id.clone(), email_id));
            }
        }
        let mut response = ObjectBuilder::new()
            .set("accountId", "account-001")
            .set("oldState", "sstate-001")
            .set("newState", "sstate-002");
        let created = created.build();
        if !created.as_object().unwrap_or(&[]).is_empty() {
            response = response.set("created", created);
        }
        let not_created = not_created.build();
        if !not_created.as_object().unwrap_or(&[]).is_empty() {
            response = response.set("notCreated", not_created);
        }
        let mut responses = vec![json!(["EmailSubmission/set", response.build(), call_id])];
        if let Some(on_success) = args.get("onSuccessUpdateEmail").and_then(Value::as_object) {
            let mut updated = ObjectBuilder::new();
            let mut not_updated = ObjectBuilder::new();
            for (reference, patch) in on_success {
                let Some((_, email_id)) = succeeded
                    .iter()
                    .find(|(cid, _)| reference.strip_prefix('#') == Some(cid.as_str()))
                else {
                    continue;
                };
                if let Some(email) = self.emails.get_mut(email_id) {
                    // A subject marked [stay] is a filing the server refuses.
                    if email.subject.contains("[stay]") {
                        not_updated.insert(
                            email_id.clone(),
                            json!({"type": "invalidPatch", "description": "kept as a draft"}),
                        );
                        continue;
                    }
                    Self::patch_email(email, patch);
                    updated.insert(email_id.clone(), Value::Null);
                }
            }
            let mut implicit = ObjectBuilder::new()
                .set("accountId", "account-001")
                .set("oldState", "estate-002")
                .set("newState", "estate-003")
                .set("updated", updated.build());
            let not_updated = not_updated.build();
            if !not_updated.as_object().unwrap_or(&[]).is_empty() {
                implicit = implicit.set("notUpdated", not_updated);
            }
            responses.push(json!(["Email/set", implicit.build(), call_id]));
        }
        responses
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

        let mut created = ObjectBuilder::new();
        if let Some(create) = args.get("create").and_then(Value::as_object) {
            for (creation_id, object) in create {
                let made = self.create_email(creation_id, object);
                created.insert(creation_id.clone(), made);
            }
        }

        if let Some(update) = args.get("update").and_then(Value::as_object) {
            for (id, patch) in update {
                let Some(email) = self.emails.get_mut(id) else {
                    not_updated.insert(id.clone(), json!({"type": "notFound"}));
                    continue;
                };
                Self::patch_email(email, patch);
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
        let created = created.build();
        let updated = updated.build();
        let not_updated = not_updated.build();
        let not_destroyed = not_destroyed.build();
        if !created.as_object().unwrap_or(&[]).is_empty() {
            resp = resp.set("created", created);
        }
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
    state: Arc<Mutex<MockState>>,
}

impl MockJmapServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let state = Arc::new(Mutex::new(MockState::new()));
        let served = Arc::clone(&state);

        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on listener");

        let handle = thread::spawn(move || {
            Self::serve(listener, shutdown_clone, served, port);
        });

        MockJmapServer {
            port,
            shutdown,
            handle: Some(handle),
            state,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every blob uploaded so far: id, type and bytes.
    #[allow(dead_code)]
    pub fn uploads(&self) -> Vec<(String, String, Vec<u8>)> {
        self.state.lock().expect("state lock").uploads.clone()
    }

    /// Every `Email/set` create so far: the id given and the object.
    #[allow(dead_code)]
    pub fn created_emails(&self) -> Vec<(String, Value)> {
        self.state.lock().expect("state lock").created.clone()
    }

    /// Every `EmailSubmission/set` create so far: the id and the object.
    #[allow(dead_code)]
    pub fn submissions(&self) -> Vec<(String, Value)> {
        self.state.lock().expect("state lock").submissions.clone()
    }

    /// A stored message's mailbox and keywords, for what a send left.
    #[allow(dead_code)]
    pub fn email_state(&self, id: &str) -> Option<(String, bool, bool)> {
        self.state
            .lock()
            .expect("state lock")
            .emails
            .get(id)
            .map(|e| (e.mailbox_id.clone(), e.is_read, e.is_draft))
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
        let mut request_type = String::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() {
                return;
            }
            let trimmed = header.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => {
                        if let Ok(len) = value.trim().parse() {
                            content_length = len;
                        }
                    }
                    "content-type" => request_type = value.trim().to_string(),
                    _ => {}
                }
            }
        }

        let raw_body = if content_length > 0 {
            let mut buf = vec![0u8; content_length];
            if reader.read_exact(&mut buf).is_err() {
                return;
            }
            buf
        } else {
            Vec::new()
        };
        let body = String::from_utf8_lossy(&raw_body).to_string();

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
            } else if method == "POST" && path.starts_with("/upload/account-001") {
                let mut guard = state.lock().expect("state lock");
                let blob_id = format!("blob-up-{}", guard.uploads.len() + 1);
                guard
                    .uploads
                    .push((blob_id.clone(), request_type.clone(), raw_body.clone()));
                let reply = json!({
                    "accountId": "account-001",
                    "blobId": blob_id,
                    "type": request_type,
                    "size": raw_body.len()
                });
                (
                    "201 Created".to_string(),
                    reply.to_string(),
                    "application/json",
                )
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
            "uploadUrl": format!("http://127.0.0.1:{}/upload/{{accountId}}/", port),
            "capabilities": {
                "urn:ietf:params:jmap:core": { "maxSizeUpload": 1000000 },
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": { "maxDelayedSend": 0 }
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "account-001",
                "urn:ietf:params:jmap:submission": "account-001"
            },
            "accounts": {
                "account-001": {
                    "name": "Test Account",
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {
                        "urn:ietf:params:jmap:mail": {},
                        "urn:ietf:params:jmap:submission": { "maxDelayedSend": 0 }
                    }
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
                            },
                            {
                                "id": "mbox-drafts",
                                "name": "Drafts",
                                "role": "drafts",
                                "totalEmails": 0,
                                "unreadEmails": 0,
                                "sortOrder": 5
                            },
                            {
                                "id": "mbox-sent",
                                "name": "Sent",
                                "role": "sent",
                                "totalEmails": 0,
                                "unreadEmails": 0,
                                "sortOrder": 6
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
                "Identity/get" => json!([
                    "Identity/get",
                    {
                        "accountId": "account-001",
                        "state": "istate-001",
                        "list": [
                            {
                                "id": "ident-001",
                                "name": "Test User",
                                "email": "test@example.com",
                                "mayDelete": false
                            }
                        ],
                        "notFound": []
                    },
                    call_id
                ]),
                "EmailSubmission/set" => {
                    let all = {
                        let mut guard = state.lock().expect("state lock");
                        guard.apply_submission_set(args, call_id)
                    };
                    responses.extend(all);
                    continue;
                }
                "EmailSubmission/query" => {
                    let filter = args.get("filter").cloned().unwrap_or_else(|| json!({}));
                    let strings = |key: &str| -> Option<Vec<String>> {
                        filter.get(key).and_then(Value::as_array).map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                    };
                    let email_ids = strings("emailIds");
                    let identity_ids = strings("identityIds");
                    let after = filter
                        .get("after")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let before = filter
                        .get("before")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let ids: Vec<String> = {
                        let guard = state.lock().expect("state lock");
                        guard
                            .submission_emails
                            .iter()
                            .filter(|(_, email_id, send_at)| {
                                email_ids.as_ref().is_none_or(|ids| ids.contains(email_id))
                                    && identity_ids
                                        .as_ref()
                                        .is_none_or(|ids| ids.iter().any(|id| id == "ident-001"))
                                    && after.as_ref().is_none_or(|after| send_at > after)
                                    && before.as_ref().is_none_or(|before| send_at < before)
                            })
                            .map(|(id, _, _)| id.clone())
                            .collect()
                    };
                    json!([
                        "EmailSubmission/query",
                        {
                            "accountId": "account-001",
                            "queryState": "sqstate-001",
                            "ids": ids,
                            "position": 0,
                            "total": ids.len()
                        },
                        call_id
                    ])
                }
                "EmailSubmission/get" => {
                    let wanted: Vec<String> = args
                        .get("ids")
                        .and_then(Value::as_array)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let (list, not_found): (Vec<Value>, Vec<String>) = {
                        let guard = state.lock().expect("state lock");
                        let mut list = Vec::new();
                        let mut not_found = Vec::new();
                        for id in wanted {
                            match guard
                                .submission_emails
                                .iter()
                                .find(|(sub_id, _, _)| *sub_id == id)
                            {
                                Some((sub_id, email_id, send_at)) => list.push(json!({
                                    "id": sub_id,
                                    "emailId": email_id,
                                    "identityId": "ident-001",
                                    "sendAt": send_at,
                                    "undoStatus": "final"
                                })),
                                None => not_found.push(id),
                            }
                        }
                        (list, not_found)
                    };
                    json!([
                        "EmailSubmission/get",
                        {
                            "accountId": "account-001",
                            "state": "sstate-002",
                            "list": list,
                            "notFound": not_found
                        },
                        call_id
                    ])
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

        // The request was served in full; only its answer is lost.
        let lose = {
            let mut guard = state.lock().expect("state lock");
            std::mem::take(&mut guard.lose_answer)
        };
        if lose {
            return (
                "502 Bad Gateway".to_string(),
                "the answer was lost".to_string(),
            );
        }

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
