use crate::cache::Cache;
use crate::config::{RetentionPolicyConfig, SpamConfig};
use crate::jmap::client::JmapClient;
use crate::jmap::types::{Email, Mailbox};
use crate::json::{self, Json, ObjectBuilder, ToJson};
use crate::regex::UserRegex;
use crate::rules::{self, CompiledRule};
use crate::spam::{self, SpamModel};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Commands sent from the UI thread to the backend thread.
pub enum BackendCommand {
    FetchMailboxes {
        origin: String,
    },
    CreateMailbox {
        name: String,
    },
    DeleteMailbox {
        id: String,
        name: String,
    },
    QueryEmails {
        origin: String,
        mailbox_id: String,
        page_size: u32,
        position: u32,
        search_query: Option<String>,
        received_after: Option<String>,
        received_before: Option<String>,
    },
    GetEmail {
        id: String,
    },
    GetEmailForReply {
        id: String,
    },
    MarkEmailRead {
        op_id: u64,
        id: String,
    },
    MarkEmailUnread {
        op_id: u64,
        id: String,
    },
    SetEmailFlagged {
        op_id: u64,
        id: String,
        flagged: bool,
    },
    MoveEmail {
        op_id: u64,
        id: String,
        to_mailbox_id: String,
    },
    MoveThread {
        op_id: u64,
        thread_id: String,
        to_mailbox_id: String,
    },
    DestroyEmail {
        op_id: u64,
        id: String,
    },
    DestroyThread {
        op_id: u64,
        thread_id: String,
    },
    QueryThreadEmails {
        thread_id: String,
    },
    MarkThreadRead {
        thread_id: String,
        email_ids: Vec<String>,
    },
    MarkMailboxRead {
        mailbox_id: String,
        mailbox_name: String,
    },
    GetEmailRawHeaders {
        id: String,
    },
    /// Fetch the full raw RFC822 message (headers + body), e.g. to forward it
    /// as a `message/rfc822` attachment.
    GetEmailRaw {
        id: String,
    },
    DownloadAttachment {
        blob_id: String,
        name: String,
        content_type: String,
    },
    PreviewRetentionExpiry {
        policies: Vec<RetentionPolicyConfig>,
    },
    ExecuteRetentionExpiry {
        policies: Vec<RetentionPolicyConfig>,
    },
    PreviewRulesForMailbox {
        origin: String,
        mailbox_id: String,
        mailbox_name: String,
    },
    RunRulesForMailbox {
        origin: String,
        mailbox_id: String,
        mailbox_name: String,
    },
    /// Train the spam classifier on a message (spam=true) or as ham (spam=false).
    TrainMessage {
        origin: String,
        id: String,
        spam: bool,
    },
    /// Bulk-train every message in a mailbox (up to `limit`, 0 = all).
    TrainMailbox {
        origin: String,
        mailbox_id: String,
        spam: bool,
        limit: u32,
    },
    /// Score a mailbox sample read-only (no actions) for validation.
    ClassifyMailbox {
        origin: String,
        mailbox_id: String,
        limit: u32,
    },
    /// Score a single message read-only (no actions), e.g. on-demand from the UI.
    ClassifyMessage {
        origin: String,
        id: String,
    },
    Shutdown,
}

/// Responses sent from the backend thread to the UI thread.
pub enum BackendResponse {
    Mailboxes(Result<Vec<Mailbox>, String>),
    MailboxCreated {
        name: String,
        result: Result<(), String>,
    },
    MailboxDeleted {
        name: String,
        result: Result<(), String>,
    },
    Emails {
        mailbox_id: String,
        emails: Result<Vec<Email>, String>,
        total: Option<u32>,
        position: u32,
        loaded: u32,
        thread_counts: HashMap<String, (usize, usize)>,
    },
    ThreadEmails {
        thread_id: String,
        emails: Result<Vec<Email>, String>,
    },
    EmailBody {
        id: String,
        result: Box<Result<Email, String>>,
    },
    EmailForReply {
        id: String,
        result: Box<Result<Email, String>>,
    },
    EmailMutation {
        op_id: u64,
        id: String,
        action: EmailMutationAction,
        result: Result<(), String>,
    },
    ThreadMarkedRead {
        #[allow(dead_code)]
        thread_id: String,
        #[allow(dead_code)]
        result: Result<(), String>,
    },
    MailboxMarkedRead {
        mailbox_id: String,
        mailbox_name: String,
        updated: usize,
        result: Result<(), String>,
    },
    EmailRawHeaders {
        id: String,
        result: Result<String, String>,
    },
    EmailRaw {
        id: String,
        result: Result<String, String>,
    },
    AttachmentDownloaded {
        name: String,
        result: Result<std::path::PathBuf, String>,
    },
    RetentionPreview {
        result: Result<RetentionPreviewResult, String>,
    },
    RetentionExecuted {
        result: Result<RetentionExecutionResult, String>,
    },
    RulesDryRun {
        mailbox_id: String,
        mailbox_name: String,
        result: Result<RulesDryRunResult, String>,
    },
    RulesRun {
        mailbox_id: String,
        mailbox_name: String,
        result: Result<RulesRunResult, String>,
    },
    MessageTrained {
        spam: bool,
        result: Result<(), String>,
    },
    MailboxTrained {
        spam: bool,
        /// Ok((trained, failed)) message counts, or an error.
        result: Result<(usize, usize), String>,
    },
    MailboxClassified {
        result: Result<Vec<ClassifyEntry>, String>,
    },
    MessageClassified {
        id: String,
        /// Ok((score, verdict)) or an error.
        result: Result<(f64, String), String>,
    },
}

/// One message's read-only classification result, used for validation.
#[derive(Clone, Debug)]
pub struct ClassifyEntry {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub score: f64,
    pub verdict: String,
}

#[derive(Clone, Debug)]
pub struct RetentionCandidate {
    pub id: String,
    pub mailbox: String,
    pub policy: String,
    pub received_at: String,
    pub from: String,
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct RetentionPreviewResult {
    pub candidates: Vec<RetentionCandidate>,
}

#[derive(Clone, Debug)]
pub struct RetentionExecutionResult {
    pub deleted: usize,
    pub failed_batches: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RulesRunResult {
    pub scanned: usize,
    pub matched_rules: usize,
    pub actions: usize,
}

#[derive(Clone, Debug)]
pub struct RulesDryRunEntry {
    pub received_at: String,
    pub from: String,
    pub subject: String,
    pub rule_name: String,
    pub actions: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RulesDryRunResult {
    pub scanned: usize,
    pub matched_rules: usize,
    pub actions: usize,
    pub entries: Vec<RulesDryRunEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailMutationAction {
    MarkRead,
    MarkUnread,
    SetFlagged(bool),
    Move,
    Destroy,
}

/// The offline queue's entries. Written as one JSON object per operation
/// carrying a `kind` discriminator beside the variant's own fields, which is
/// the shape serde's internally-tagged representation wrote, so a queue
/// written by an older build still replays.
#[derive(Clone, Debug)]
enum QueuedMutation {
    MarkRead {
        op_id: u64,
        id: String,
    },
    MarkUnread {
        op_id: u64,
        id: String,
    },
    SetFlagged {
        op_id: u64,
        id: String,
        flagged: bool,
    },
    MoveEmail {
        op_id: u64,
        id: String,
        to_mailbox_id: String,
    },
    MoveThread {
        op_id: u64,
        thread_id: String,
        to_mailbox_id: String,
    },
    DestroyEmail {
        op_id: u64,
        id: String,
    },
    DestroyThread {
        op_id: u64,
        thread_id: String,
    },
    MarkThreadRead {
        thread_id: String,
        email_ids: Vec<String>,
    },
    MarkMailboxRead {
        mailbox_id: String,
        mailbox_name: String,
    },
    RunRulesForMailbox {
        mailbox_id: String,
    },
    ExecuteRetentionExpiry {
        policies: Vec<RetentionPolicySnapshot>,
    },
}

impl ToJson for QueuedMutation {
    fn to_json(&self) -> Json {
        let tagged = |kind: &str, fields: ObjectBuilder| -> Json {
            let mut obj = ObjectBuilder::new().set("kind", kind);
            for (key, value) in fields.into_pairs() {
                obj = obj.set(key, value);
            }
            obj.build()
        };
        match self {
            QueuedMutation::MarkRead { op_id, id } => tagged(
                "mark_read",
                ObjectBuilder::new().set("op_id", *op_id).set("id", id),
            ),
            QueuedMutation::MarkUnread { op_id, id } => tagged(
                "mark_unread",
                ObjectBuilder::new().set("op_id", *op_id).set("id", id),
            ),
            QueuedMutation::SetFlagged { op_id, id, flagged } => tagged(
                "set_flagged",
                ObjectBuilder::new()
                    .set("op_id", *op_id)
                    .set("id", id)
                    .set("flagged", *flagged),
            ),
            QueuedMutation::MoveEmail {
                op_id,
                id,
                to_mailbox_id,
            } => tagged(
                "move_email",
                ObjectBuilder::new()
                    .set("op_id", *op_id)
                    .set("id", id)
                    .set("to_mailbox_id", to_mailbox_id),
            ),
            QueuedMutation::MoveThread {
                op_id,
                thread_id,
                to_mailbox_id,
            } => tagged(
                "move_thread",
                ObjectBuilder::new()
                    .set("op_id", *op_id)
                    .set("thread_id", thread_id)
                    .set("to_mailbox_id", to_mailbox_id),
            ),
            QueuedMutation::DestroyEmail { op_id, id } => tagged(
                "destroy_email",
                ObjectBuilder::new().set("op_id", *op_id).set("id", id),
            ),
            QueuedMutation::DestroyThread { op_id, thread_id } => tagged(
                "destroy_thread",
                ObjectBuilder::new()
                    .set("op_id", *op_id)
                    .set("thread_id", thread_id),
            ),
            QueuedMutation::MarkThreadRead {
                thread_id,
                email_ids,
            } => tagged(
                "mark_thread_read",
                ObjectBuilder::new()
                    .set("thread_id", thread_id)
                    .set("email_ids", email_ids),
            ),
            QueuedMutation::MarkMailboxRead {
                mailbox_id,
                mailbox_name,
            } => tagged(
                "mark_mailbox_read",
                ObjectBuilder::new()
                    .set("mailbox_id", mailbox_id)
                    .set("mailbox_name", mailbox_name),
            ),
            QueuedMutation::RunRulesForMailbox { mailbox_id } => tagged(
                "run_rules_for_mailbox",
                ObjectBuilder::new().set("mailbox_id", mailbox_id),
            ),
            QueuedMutation::ExecuteRetentionExpiry { policies } => tagged(
                "execute_retention_expiry",
                ObjectBuilder::new().set("policies", policies),
            ),
        }
    }
}

/// One queue entry's fields, named as serde named them in its message.
fn queue_str(obj: &Json, field: &str) -> Result<String, String> {
    obj.get(field)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing field `{}`", field))
}

fn queue_u64(obj: &Json, field: &str) -> Result<u64, String> {
    obj.get(field)
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("missing field `{}`", field))
}

fn queue_bool(obj: &Json, field: &str) -> Result<bool, String> {
    obj.get(field)
        .and_then(Json::as_bool)
        .ok_or_else(|| format!("missing field `{}`", field))
}

fn queue_strs(obj: &Json, field: &str) -> Result<Vec<String>, String> {
    obj.get(field)
        .and_then(Json::as_arr)
        .ok_or_else(|| format!("missing field `{}`", field))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("invalid type for `{}`", field))
        })
        .collect()
}

impl QueuedMutation {
    fn from_json(value: &Json) -> Result<Self, String> {
        let kind = value
            .get("kind")
            .and_then(Json::as_str)
            .ok_or_else(|| "missing field `kind`".to_string())?;
        match kind {
            "mark_read" => Ok(QueuedMutation::MarkRead {
                op_id: queue_u64(value, "op_id")?,
                id: queue_str(value, "id")?,
            }),
            "mark_unread" => Ok(QueuedMutation::MarkUnread {
                op_id: queue_u64(value, "op_id")?,
                id: queue_str(value, "id")?,
            }),
            "set_flagged" => Ok(QueuedMutation::SetFlagged {
                op_id: queue_u64(value, "op_id")?,
                id: queue_str(value, "id")?,
                flagged: queue_bool(value, "flagged")?,
            }),
            "move_email" => Ok(QueuedMutation::MoveEmail {
                op_id: queue_u64(value, "op_id")?,
                id: queue_str(value, "id")?,
                to_mailbox_id: queue_str(value, "to_mailbox_id")?,
            }),
            "move_thread" => Ok(QueuedMutation::MoveThread {
                op_id: queue_u64(value, "op_id")?,
                thread_id: queue_str(value, "thread_id")?,
                to_mailbox_id: queue_str(value, "to_mailbox_id")?,
            }),
            "destroy_email" => Ok(QueuedMutation::DestroyEmail {
                op_id: queue_u64(value, "op_id")?,
                id: queue_str(value, "id")?,
            }),
            "destroy_thread" => Ok(QueuedMutation::DestroyThread {
                op_id: queue_u64(value, "op_id")?,
                thread_id: queue_str(value, "thread_id")?,
            }),
            "mark_thread_read" => Ok(QueuedMutation::MarkThreadRead {
                thread_id: queue_str(value, "thread_id")?,
                email_ids: queue_strs(value, "email_ids")?,
            }),
            "mark_mailbox_read" => Ok(QueuedMutation::MarkMailboxRead {
                mailbox_id: queue_str(value, "mailbox_id")?,
                mailbox_name: queue_str(value, "mailbox_name")?,
            }),
            "run_rules_for_mailbox" => Ok(QueuedMutation::RunRulesForMailbox {
                mailbox_id: queue_str(value, "mailbox_id")?,
            }),
            "execute_retention_expiry" => {
                let policies = value
                    .get("policies")
                    .and_then(Json::as_arr)
                    .ok_or_else(|| "missing field `policies`".to_string())?
                    .iter()
                    .map(RetentionPolicySnapshot::from_json)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(QueuedMutation::ExecuteRetentionExpiry { policies })
            }
            other => Err(format!("unknown variant `{}`", other)),
        }
    }
}

static GENERATED_OP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
struct RetentionPolicySnapshot {
    name: String,
    folder: String,
    days: u32,
}

impl ToJson for RetentionPolicySnapshot {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("name", &self.name)
            .set("folder", &self.folder)
            .set("days", self.days)
            .build()
    }
}

impl RetentionPolicySnapshot {
    fn from_json(value: &Json) -> Result<Self, String> {
        let days = value
            .get("days")
            .and_then(Json::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "missing field `days`".to_string())?;
        Ok(RetentionPolicySnapshot {
            name: queue_str(value, "name")?,
            folder: queue_str(value, "folder")?,
            days,
        })
    }
}

impl From<&RetentionPolicyConfig> for RetentionPolicySnapshot {
    fn from(value: &RetentionPolicyConfig) -> Self {
        Self {
            name: value.name.clone(),
            folder: value.folder.clone(),
            days: value.days,
        }
    }
}

impl From<&RetentionPolicySnapshot> for RetentionPolicyConfig {
    fn from(value: &RetentionPolicySnapshot) -> Self {
        Self {
            name: value.name.clone(),
            folder: value.folder.clone(),
            days: value.days,
        }
    }
}

fn queue_mutation(cache: Option<&Cache>, op: &QueuedMutation) -> Result<u64, String> {
    let cache = cache.ok_or_else(|| "cache unavailable (offline mode)".to_string())?;
    // Writing a value cannot fail, so queueing has no serialize error.
    cache.enqueue_operation(&op.to_json().to_vec())
}

fn apply_local_mutation(cache: Option<&Cache>, op: &QueuedMutation) {
    let Some(cache) = cache else {
        return;
    };
    match op {
        QueuedMutation::MarkRead { id, .. } => {
            let _ = cache.apply_mark_seen(id, true);
        }
        QueuedMutation::MarkUnread { id, .. } => {
            let _ = cache.apply_mark_seen(id, false);
        }
        QueuedMutation::SetFlagged { id, flagged, .. } => {
            let _ = cache.apply_set_flagged(id, *flagged);
        }
        QueuedMutation::MoveEmail {
            id, to_mailbox_id, ..
        } => {
            let _ = cache.apply_move_email(id, to_mailbox_id);
        }
        QueuedMutation::MoveThread {
            thread_id,
            to_mailbox_id,
            ..
        } => {
            for email in cache.get_thread_emails(thread_id) {
                let _ = cache.apply_move_email(&email.id, to_mailbox_id);
            }
        }
        QueuedMutation::DestroyEmail { id, .. } => {
            let _ = cache.apply_destroy_email(id);
        }
        QueuedMutation::DestroyThread { thread_id, .. } => {
            for email in cache.get_thread_emails(thread_id) {
                let _ = cache.apply_destroy_email(&email.id);
            }
        }
        QueuedMutation::MarkThreadRead { email_ids, .. } => {
            for id in email_ids {
                let _ = cache.apply_mark_seen(id, true);
            }
        }
        QueuedMutation::MarkMailboxRead { mailbox_id, .. } => {
            let _ = cache.apply_mark_mailbox_read(mailbox_id);
        }
        QueuedMutation::RunRulesForMailbox { .. }
        | QueuedMutation::ExecuteRetentionExpiry { .. } => {}
    }
}

fn is_missing_remote_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("not found")
        || lower.contains("notfound")
        || lower.contains("unknown")
        || lower.contains("invalid email id")
}

#[allow(clippy::too_many_arguments)]
fn process_mutation_via_queue(
    client: &JmapClient,
    op: &QueuedMutation,
    cached_mailboxes: &mut Vec<Mailbox>,
    rules: &[CompiledRule],
    custom_headers: &[String],
    my_email_regex: &UserRegex,
    cache: Option<&Cache>,
) -> Result<(), String> {
    let Some(cache) = cache else {
        log_warn!(
            "[Backend] cache unavailable; executing mutation without durable queue: {:?}",
            op
        );
        return execute_remote_mutation(
            client,
            op,
            cached_mailboxes,
            rules,
            custom_headers,
            my_email_regex,
            None,
        );
    };
    let seq = queue_mutation(Some(cache), op)?;
    apply_local_mutation(Some(cache), op);

    match execute_remote_mutation(
        client,
        op,
        cached_mailboxes,
        rules,
        custom_headers,
        my_email_regex,
        Some(cache),
    ) {
        Ok(()) => {
            let _ = cache.remove_queued_operation(seq);
        }
        Err(e) => {
            let resolved = matches!(
                op,
                QueuedMutation::MoveEmail { .. }
                    | QueuedMutation::MoveThread { .. }
                    | QueuedMutation::DestroyEmail { .. }
                    | QueuedMutation::DestroyThread { .. }
            ) && is_missing_remote_error(&e);
            if resolved {
                log_info!(
                    "[Backend] queue op seq={} resolved by conflict policy: {}",
                    seq,
                    e
                );
                let _ = cache.remove_queued_operation(seq);
            } else {
                log_warn!(
                    "[Backend] queued op seq={} deferred for replay kind={:?}: {}",
                    seq,
                    op,
                    e
                );
            }
        }
    }
    Ok(())
}

/// Spawn the backend thread. Returns the command sender and response receiver.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    client: Option<JmapClient>,
    account_name: String,
    rules: Arc<Vec<CompiledRule>>,
    custom_headers: Arc<Vec<String>>,
    rules_mailbox_regex: Arc<UserRegex>,
    my_email_regex: Arc<UserRegex>,
    spam_config: SpamConfig,
) -> (
    mpsc::Sender<BackendCommand>,
    mpsc::Receiver<BackendResponse>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
    let (resp_tx, resp_rx) = mpsc::channel::<BackendResponse>();

    thread::spawn(move || {
        let cache = match Cache::open(&account_name) {
            Ok(c) => {
                log_info!("[Backend] Opened cache for account '{}'", account_name);
                Some(c)
            }
            Err(e) => {
                log_debug!(
                    "[Backend] Cache unavailable for '{}': {} (proceeding without cache)",
                    account_name,
                    e
                );
                None
            }
        };
        let spam_model = SpamModel::load(&spam::model_path());
        log_info!(
            "[Spam] loaded model ({} spam / {} ham trained, enabled={})",
            spam_model.spam_messages(),
            spam_model.ham_messages(),
            spam_config.enabled
        );
        backend_loop(
            client,
            cmd_rx,
            resp_tx,
            rules,
            custom_headers,
            rules_mailbox_regex,
            my_email_regex,
            cache,
            spam_config,
            spam_model,
        );
    });

    (cmd_tx, resp_rx)
}

/// Handle a command in offline mode. Returns true to continue, false to break (shutdown).
fn handle_offline_command(
    cmd: &BackendCommand,
    resp_tx: &mpsc::Sender<BackendResponse>,
    cache: &Option<Cache>,
    cached_mailboxes: &mut Vec<Mailbox>,
    command_seq: u64,
) -> bool {
    match cmd {
        BackendCommand::FetchMailboxes { origin } => {
            log_info!(
                "[Backend/offline] cmd#{} FetchMailboxes origin='{}'",
                command_seq,
                origin
            );
            let result = if let Some(ref cache) = cache {
                if let Some(mboxes) = cache.get_mailboxes() {
                    if !mboxes.is_empty() {
                        *cached_mailboxes = mboxes.clone();
                        Ok(mboxes)
                    } else {
                        Err("no cached mailboxes available (offline mode)".to_string())
                    }
                } else {
                    Err("no cached mailboxes available (offline mode)".to_string())
                }
            } else {
                Err("cache unavailable (offline mode)".to_string())
            };
            let _ = resp_tx.send(BackendResponse::Mailboxes(result));
        }
        BackendCommand::QueryEmails {
            origin,
            mailbox_id,
            position,
            search_query,
            received_after,
            received_before,
            ..
        } => {
            log_info!(
                "[Backend/offline] cmd#{} QueryEmails origin='{}' mailbox_id='{}'",
                command_seq,
                origin,
                mailbox_id
            );
            // Only serve first page, no search/date filters
            if *position != 0
                || search_query.is_some()
                || received_after.is_some()
                || received_before.is_some()
            {
                let _ = resp_tx.send(BackendResponse::Emails {
                    mailbox_id: mailbox_id.clone(),
                    emails: Err("search and pagination not available in offline mode".to_string()),
                    total: None,
                    position: *position,
                    loaded: 0,
                    thread_counts: HashMap::new(),
                });
            } else if let Some(ref cache) = cache {
                if let Some(cached_emails) = cache.get_mailbox_emails(mailbox_id) {
                    let loaded = cached_emails.len() as u32;
                    let _ = resp_tx.send(BackendResponse::Emails {
                        mailbox_id: mailbox_id.clone(),
                        emails: Ok(cached_emails),
                        total: None,
                        position: 0,
                        loaded,
                        thread_counts: HashMap::new(),
                    });
                } else {
                    let _ = resp_tx.send(BackendResponse::Emails {
                        mailbox_id: mailbox_id.clone(),
                        emails: Ok(Vec::new()),
                        total: Some(0),
                        position: 0,
                        loaded: 0,
                        thread_counts: HashMap::new(),
                    });
                }
            } else {
                let _ = resp_tx.send(BackendResponse::Emails {
                    mailbox_id: mailbox_id.clone(),
                    emails: Err("cache unavailable (offline mode)".to_string()),
                    total: None,
                    position: 0,
                    loaded: 0,
                    thread_counts: HashMap::new(),
                });
            }
        }
        BackendCommand::GetEmail { id } => {
            let result = if let Some(ref cache) = cache {
                if let Some(email) = cache.get_email(id) {
                    log_debug!("[Backend/offline] Cache hit for email {}", id);
                    Ok(email)
                } else {
                    Err("email not cached (offline mode)".to_string())
                }
            } else {
                Err("cache unavailable (offline mode)".to_string())
            };
            let _ = resp_tx.send(BackendResponse::EmailBody {
                id: id.clone(),
                result: Box::new(result),
            });
        }
        BackendCommand::GetEmailForReply { id } => {
            let result = if let Some(ref cache) = cache {
                if let Some(email) = cache.get_email(id) {
                    Ok(email)
                } else {
                    Err("email not cached (offline mode)".to_string())
                }
            } else {
                Err("cache unavailable (offline mode)".to_string())
            };
            let _ = resp_tx.send(BackendResponse::EmailForReply {
                id: id.clone(),
                result: Box::new(result),
            });
        }
        BackendCommand::Shutdown => {
            return false;
        }
        // Offline mutations: queue and apply local projection.
        BackendCommand::MarkEmailRead { op_id, id, .. } => {
            let op = QueuedMutation::MarkRead {
                op_id: *op_id,
                id: id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: id.clone(),
                action: EmailMutationAction::MarkRead,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::MarkEmailUnread { op_id, id, .. } => {
            let op = QueuedMutation::MarkUnread {
                op_id: *op_id,
                id: id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: id.clone(),
                action: EmailMutationAction::MarkUnread,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::SetEmailFlagged {
            op_id, id, flagged, ..
        } => {
            let op = QueuedMutation::SetFlagged {
                op_id: *op_id,
                id: id.clone(),
                flagged: *flagged,
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: id.clone(),
                action: EmailMutationAction::SetFlagged(*flagged),
                result: result.map(|_| ()),
            });
        }
        BackendCommand::MoveEmail {
            op_id,
            id,
            to_mailbox_id,
        } => {
            let op = QueuedMutation::MoveEmail {
                op_id: *op_id,
                id: id.clone(),
                to_mailbox_id: to_mailbox_id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: id.clone(),
                action: EmailMutationAction::Move,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::MoveThread {
            op_id,
            thread_id,
            to_mailbox_id,
        } => {
            let op = QueuedMutation::MoveThread {
                op_id: *op_id,
                thread_id: thread_id.clone(),
                to_mailbox_id: to_mailbox_id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: thread_id.clone(),
                action: EmailMutationAction::Move,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::DestroyEmail { op_id, id, .. } => {
            let op = QueuedMutation::DestroyEmail {
                op_id: *op_id,
                id: id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: id.clone(),
                action: EmailMutationAction::Destroy,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::DestroyThread {
            op_id, thread_id, ..
        } => {
            let op = QueuedMutation::DestroyThread {
                op_id: *op_id,
                thread_id: thread_id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::EmailMutation {
                op_id: *op_id,
                id: thread_id.clone(),
                action: EmailMutationAction::Destroy,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::CreateMailbox { name } => {
            let _ = resp_tx.send(BackendResponse::MailboxCreated {
                name: name.clone(),
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::DeleteMailbox { name, .. } => {
            let _ = resp_tx.send(BackendResponse::MailboxDeleted {
                name: name.clone(),
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::QueryThreadEmails { thread_id } => {
            let result = if let Some(cache) = cache {
                Ok(cache.get_thread_emails(thread_id))
            } else {
                Err("cache unavailable (offline mode)".to_string())
            };
            let _ = resp_tx.send(BackendResponse::ThreadEmails {
                thread_id: thread_id.clone(),
                emails: result,
            });
        }
        BackendCommand::MarkThreadRead {
            thread_id,
            email_ids,
        } => {
            let op = QueuedMutation::MarkThreadRead {
                thread_id: thread_id.clone(),
                email_ids: email_ids.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::ThreadMarkedRead {
                thread_id: thread_id.clone(),
                result: result.map(|_| ()),
            });
        }
        BackendCommand::MarkMailboxRead {
            mailbox_id,
            mailbox_name,
        } => {
            let op = QueuedMutation::MarkMailboxRead {
                mailbox_id: mailbox_id.clone(),
                mailbox_name: mailbox_name.clone(),
            };
            let local_updated = cache
                .as_ref()
                .and_then(|c| c.get_mailbox_emails(mailbox_id))
                .map(|emails| {
                    emails
                        .iter()
                        .filter(|e| !e.keywords.contains_key("$seen"))
                        .count()
                })
                .unwrap_or(0);
            let result = queue_mutation(cache.as_ref(), &op).map(|_| {
                apply_local_mutation(cache.as_ref(), &op);
            });
            let _ = resp_tx.send(BackendResponse::MailboxMarkedRead {
                mailbox_id: mailbox_id.clone(),
                mailbox_name: mailbox_name.clone(),
                updated: local_updated,
                result: result.map(|_| ()),
            });
        }
        BackendCommand::GetEmailRawHeaders { id } => {
            let _ = resp_tx.send(BackendResponse::EmailRawHeaders {
                id: id.clone(),
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::GetEmailRaw { id } => {
            let _ = resp_tx.send(BackendResponse::EmailRaw {
                id: id.clone(),
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::DownloadAttachment { name, .. } => {
            let _ = resp_tx.send(BackendResponse::AttachmentDownloaded {
                name: name.clone(),
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::PreviewRetentionExpiry { .. } => {
            let _ = resp_tx.send(BackendResponse::RetentionPreview {
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::ExecuteRetentionExpiry { policies } => {
            let op = QueuedMutation::ExecuteRetentionExpiry {
                policies: policies.iter().map(RetentionPolicySnapshot::from).collect(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| ());
            let _ = resp_tx.send(BackendResponse::RetentionExecuted {
                result: result.map(|_| RetentionExecutionResult {
                    deleted: 0,
                    failed_batches: Vec::new(),
                }),
            });
        }
        BackendCommand::PreviewRulesForMailbox {
            mailbox_id,
            mailbox_name,
            ..
        } => {
            let _ = resp_tx.send(BackendResponse::RulesDryRun {
                mailbox_id: mailbox_id.clone(),
                mailbox_name: mailbox_name.clone(),
                result: Err("not available in offline mode".to_string()),
            });
        }
        BackendCommand::RunRulesForMailbox {
            mailbox_id,
            mailbox_name,
            ..
        } => {
            let op = QueuedMutation::RunRulesForMailbox {
                mailbox_id: mailbox_id.clone(),
            };
            let result = queue_mutation(cache.as_ref(), &op).map(|_| ());
            let _ = resp_tx.send(BackendResponse::RulesRun {
                mailbox_id: mailbox_id.clone(),
                mailbox_name: mailbox_name.clone(),
                result: result.map(|_| RulesRunResult {
                    scanned: 0,
                    matched_rules: 0,
                    actions: 0,
                }),
            });
        }
        BackendCommand::TrainMessage { spam, .. } => {
            // Training fetches the raw message over the network; unavailable offline.
            let _ = resp_tx.send(BackendResponse::MessageTrained {
                spam: *spam,
                result: Err("training requires an online connection".to_string()),
            });
        }
        BackendCommand::TrainMailbox { spam, .. } => {
            let _ = resp_tx.send(BackendResponse::MailboxTrained {
                spam: *spam,
                result: Err("training requires an online connection".to_string()),
            });
        }
        BackendCommand::ClassifyMailbox { .. } => {
            let _ = resp_tx.send(BackendResponse::MailboxClassified {
                result: Err("classification requires an online connection".to_string()),
            });
        }
        BackendCommand::ClassifyMessage { id, .. } => {
            let _ = resp_tx.send(BackendResponse::MessageClassified {
                id: id.clone(),
                result: Err("classification requires an online connection".to_string()),
            });
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn execute_remote_mutation(
    client: &JmapClient,
    op: &QueuedMutation,
    cached_mailboxes: &mut Vec<Mailbox>,
    rules: &[CompiledRule],
    custom_headers: &[String],
    my_email_regex: &UserRegex,
    cache: Option<&Cache>,
) -> Result<(), String> {
    match op {
        QueuedMutation::MarkRead { id, .. } => {
            client.mark_email_read(id).map_err(|e| e.to_string())
        }
        QueuedMutation::MarkUnread { id, .. } => {
            client.mark_email_unread(id).map_err(|e| e.to_string())
        }
        QueuedMutation::SetFlagged { id, flagged, .. } => client
            .set_email_flagged(id, *flagged)
            .map_err(|e| e.to_string()),
        QueuedMutation::MoveEmail {
            id, to_mailbox_id, ..
        } => client
            .move_email(id, to_mailbox_id)
            .map_err(|e| e.to_string()),
        QueuedMutation::MoveThread {
            thread_id,
            to_mailbox_id,
            ..
        } => {
            let emails = client
                .query_thread_emails(thread_id)
                .map_err(|e| e.to_string())?;
            for email in emails {
                client
                    .move_email(&email.id, to_mailbox_id)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        QueuedMutation::DestroyEmail { id, .. } => client
            .destroy_emails(std::slice::from_ref(id))
            .map_err(|e| e.to_string()),
        QueuedMutation::DestroyThread { thread_id, .. } => {
            let emails = client
                .query_thread_emails(thread_id)
                .map_err(|e| e.to_string())?;
            let ids: Vec<String> = emails.into_iter().map(|e| e.id).collect();
            client.destroy_emails(&ids).map_err(|e| e.to_string())
        }
        QueuedMutation::MarkThreadRead { email_ids, .. } => {
            if email_ids.is_empty() {
                Ok(())
            } else {
                client
                    .mark_emails_read(email_ids)
                    .map_err(|e| e.to_string())
            }
        }
        QueuedMutation::MarkMailboxRead { mailbox_id, .. } => {
            let ids = fetch_all_mailbox_email_ids(client, mailbox_id)?;
            if ids.is_empty() {
                Ok(())
            } else {
                client.mark_emails_read(&ids).map_err(|e| e.to_string())
            }
        }
        QueuedMutation::RunRulesForMailbox { mailbox_id } => {
            // Offline-replay path: skip spam scoring (None); the model and config
            // are only threaded into the live command handlers below.
            run_rules_for_mailbox(
                client,
                cached_mailboxes,
                rules,
                custom_headers,
                my_email_regex,
                mailbox_id,
                cache,
                None,
            )?;
            Ok(())
        }
        QueuedMutation::ExecuteRetentionExpiry { policies } => {
            let policies: Vec<RetentionPolicyConfig> =
                policies.iter().map(RetentionPolicyConfig::from).collect();
            let candidates = collect_retention_candidates(client, cached_mailboxes, &policies)?;
            let ops = queued_mutations_for_retention(&candidates);
            for op in &ops {
                process_mutation_via_queue(
                    client,
                    op,
                    cached_mailboxes,
                    rules,
                    custom_headers,
                    my_email_regex,
                    cache,
                )?;
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn replay_queued_mutations(
    client: &JmapClient,
    cached_mailboxes: &mut Vec<Mailbox>,
    rules: &[CompiledRule],
    custom_headers: &[String],
    my_email_regex: &UserRegex,
    cache: &Cache,
) {
    let queued = cache.queued_operations();
    if queued.is_empty() {
        return;
    }
    log_info!(
        "[Backend] Replaying {} queued offline operation(s)",
        queued.len()
    );

    for (seq, payload) in queued {
        let parsed = json::parse_slice(&payload)
            .map_err(|e| e.to_string())
            .and_then(|value| QueuedMutation::from_json(&value));
        let op = match parsed {
            Ok(op) => op,
            Err(e) => {
                log_warn!(
                    "[Backend] dropping malformed queued op seq={} parse error={}",
                    seq,
                    e
                );
                let _ = cache.remove_queued_operation(seq);
                continue;
            }
        };

        let result = execute_remote_mutation(
            client,
            &op,
            cached_mailboxes,
            rules,
            custom_headers,
            my_email_regex,
            Some(cache),
        );

        match result {
            Ok(()) => {
                let _ = cache.remove_queued_operation(seq);
            }
            Err(e) => {
                let resolved = matches!(
                    op,
                    QueuedMutation::MoveEmail { .. }
                        | QueuedMutation::MoveThread { .. }
                        | QueuedMutation::DestroyEmail { .. }
                        | QueuedMutation::DestroyThread { .. }
                ) && is_missing_remote_error(&e);
                if resolved {
                    log_info!(
                        "[Backend] queue op seq={} resolved by conflict policy: {}",
                        seq,
                        e
                    );
                    let _ = cache.remove_queued_operation(seq);
                    continue;
                }
                log_warn!(
                    "[Backend] queue replay stopped at seq={} kind={:?}: {}",
                    seq,
                    op,
                    e
                );
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn backend_loop(
    client: Option<JmapClient>,
    cmd_rx: mpsc::Receiver<BackendCommand>,
    resp_tx: mpsc::Sender<BackendResponse>,
    rules: Arc<Vec<CompiledRule>>,
    custom_headers: Arc<Vec<String>>,
    rules_mailbox_regex: Arc<UserRegex>,
    my_email_regex: Arc<UserRegex>,
    cache: Option<Cache>,
    spam_config: SpamConfig,
    mut spam_model: SpamModel,
) {
    let mut cached_mailboxes: Vec<Mailbox> = Vec::new();
    let mut command_seq: u64 = 0;
    let offline = client.is_none();
    if let Some(cache) = cache.as_ref() {
        if let Some(mboxes) = cache.get_mailboxes() {
            cached_mailboxes = mboxes;
        }
    }
    if !offline {
        if let (Some(client), Some(cache)) = (client.as_ref(), cache.as_ref()) {
            replay_queued_mutations(
                client,
                &mut cached_mailboxes,
                &rules,
                &custom_headers,
                &my_email_regex,
                cache,
            );
        }
    }

    while let Ok(cmd) = cmd_rx.recv() {
        command_seq = command_seq.wrapping_add(1);

        if offline {
            if handle_offline_command(&cmd, &resp_tx, &cache, &mut cached_mailboxes, command_seq) {
                continue;
            } else {
                break; // Shutdown
            }
        }

        // `offline` is `client.is_none()`, so this arm always has one. A `let
        // else` says that to the compiler; stopping is the graceful answer if
        // the two ever drift apart, since every command here needs a client.
        let Some(client) = client.as_ref() else {
            log_error!("[Backend] online loop reached without a client; stopping");
            break;
        };

        match cmd {
            BackendCommand::FetchMailboxes { origin } => {
                log_info!(
                    "[Backend] cmd#{} FetchMailboxes origin='{}'",
                    command_seq,
                    origin
                );

                // For TUI, serve cached mailboxes instantly then follow with live data.
                // CLI expects one response per command, so skip the cached pre-response there.
                if !origin.starts_with("cli") {
                    if let Some(ref cache) = cache {
                        if let Some(cached_mboxes) = cache.get_mailboxes() {
                            if !cached_mboxes.is_empty() {
                                log_info!(
                                    "[Backend] Serving {} cached mailboxes",
                                    cached_mboxes.len()
                                );
                                cached_mailboxes = cached_mboxes.clone();
                                let _ = resp_tx.send(BackendResponse::Mailboxes(Ok(cached_mboxes)));
                            }
                        }
                    }
                }

                let result = client.get_mailboxes().map_err(|e| e.to_string());
                if let Ok(ref mailboxes) = result {
                    cached_mailboxes = mailboxes.clone();
                    if let Some(ref cache) = cache {
                        cache.put_mailboxes(mailboxes);
                    }
                }
                let _ = resp_tx.send(BackendResponse::Mailboxes(result));
            }
            BackendCommand::CreateMailbox { name } => {
                let result = client.create_mailbox(&name).map_err(|e| e.to_string());
                if result.is_ok() {
                    if let Ok(mailboxes) = client.get_mailboxes() {
                        cached_mailboxes = mailboxes;
                        if let Some(ref cache) = cache {
                            cache.put_mailboxes(&cached_mailboxes);
                        }
                    }
                }
                let _ = resp_tx.send(BackendResponse::MailboxCreated { name, result });
            }
            BackendCommand::DeleteMailbox { id, name } => {
                let result = client.delete_mailbox(&id).map_err(|e| e.to_string());
                if result.is_ok() {
                    if let Ok(mailboxes) = client.get_mailboxes() {
                        cached_mailboxes = mailboxes;
                        if let Some(ref cache) = cache {
                            cache.put_mailboxes(&cached_mailboxes);
                        }
                    }
                }
                let _ = resp_tx.send(BackendResponse::MailboxDeleted { name, result });
            }
            BackendCommand::QueryEmails {
                origin,
                mailbox_id,
                page_size,
                position,
                search_query,
                received_after,
                received_before,
            } => {
                log_info!(
                    "[Backend] cmd#{} QueryEmails origin='{}' mailbox_id='{}' page_size={} position={} search={:?} after={:?} before={:?}",
                    command_seq,
                    origin,
                    mailbox_id,
                    page_size,
                    position,
                    search_query,
                    received_after,
                    received_before
                );

                // For TUI open flows, serve cached mailbox emails immediately so
                // folder open is instant. CLI expects one response per command.
                let can_serve_cached_first = !origin.starts_with("cli")
                    && position == 0
                    && search_query.is_none()
                    && received_after.is_none()
                    && received_before.is_none();
                if can_serve_cached_first {
                    if let Some(ref cache) = cache {
                        if let Some(cached_emails) = cache.get_mailbox_emails(&mailbox_id) {
                            let cached_total = cached_mailboxes
                                .iter()
                                .find(|m| m.id == mailbox_id)
                                .map(|m| m.total_emails);
                            let cached_loaded = cached_emails.len() as u32;
                            let _ = resp_tx.send(BackendResponse::Emails {
                                mailbox_id: mailbox_id.clone(),
                                emails: Ok(cached_emails),
                                total: cached_total,
                                position: 0,
                                loaded: cached_loaded,
                                thread_counts: HashMap::new(),
                            });
                        }
                    }
                }

                let result = (|| {
                    let query = client
                        .query_emails(
                            &mailbox_id,
                            page_size,
                            position,
                            search_query.as_deref(),
                            received_after.as_deref(),
                            received_before.as_deref(),
                        )
                        .map_err(|e| e.to_string())?;
                    let total = query.total;
                    let position = query.position;
                    let loaded = query.ids.len() as u32;
                    let mut emails = if query.ids.is_empty() {
                        Ok(Vec::new())
                    } else {
                        fetch_emails_chunked(client, &query.ids, &custom_headers)
                    }?;

                    // Cache fetched emails
                    if let Some(ref cache) = cache {
                        cache.put_emails(&emails);
                        // Update mailbox index for first-page non-search queries
                        if position == 0
                            && search_query.is_none()
                            && received_after.is_none()
                            && received_before.is_none()
                        {
                            let ids: Vec<String> = emails.iter().map(|e| e.id.clone()).collect();
                            cache.put_mailbox_index(&mailbox_id, &ids);
                        }
                    }

                    // Apply filtering rules (only to unprocessed emails)
                    if !rules.is_empty() {
                        let mailbox_name = cached_mailboxes
                            .iter()
                            .find(|m| m.id.as_str() == mailbox_id.as_str())
                            .map(|m| m.name.clone())
                            .unwrap_or_default();
                        if rules_mailbox_regex.is_match(&mailbox_name) {
                            let mut emails_for_rules = if let Some(ref cache) = cache {
                                let all_ids: Vec<String> =
                                    emails.iter().map(|e| e.id.clone()).collect();
                                let unprocessed_ids = cache.filter_unprocessed(&all_ids);
                                if unprocessed_ids.len() < emails.len() {
                                    log_info!(
                                        "[Rules] Filtered {}/{} emails as already processed",
                                        emails.len() - unprocessed_ids.len(),
                                        emails.len()
                                    );
                                }
                                let unprocessed_set: HashSet<&str> =
                                    unprocessed_ids.iter().map(|s| s.as_str()).collect();
                                emails
                                    .iter()
                                    .filter(|e| unprocessed_set.contains(e.id.as_str()))
                                    .cloned()
                                    .collect::<Vec<_>>()
                            } else {
                                emails.clone()
                            };

                            if !emails_for_rules.is_empty() {
                                let scorer = SpamScorer {
                                    config: &spam_config,
                                    model: &spam_model,
                                };
                                annotate_inbox_spam(
                                    client,
                                    Some(&scorer),
                                    &cached_mailboxes,
                                    &mailbox_id,
                                    &mut emails_for_rules,
                                );
                                let applications = rules::apply_rules(
                                    &rules,
                                    &emails_for_rules,
                                    &cached_mailboxes,
                                    &my_email_regex,
                                );
                                if !applications.is_empty() {
                                    log_info!(
                                        "[Rules] Applying {} rule action(s) to fetched emails in mailbox '{}' (query origin='{}')",
                                        applications.len(),
                                        mailbox_name,
                                        origin
                                    );
                                    let ops = queued_mutations_for_rule_actions(
                                        &applications,
                                        &cached_mailboxes,
                                    );
                                    let mut removed_ids = HashSet::new();
                                    for op in &ops {
                                        if let QueuedMutation::MoveEmail { id, .. } = op {
                                            removed_ids.insert(id.clone());
                                        }
                                        if let Err(e) = process_mutation_via_queue(
                                            client,
                                            op,
                                            &mut cached_mailboxes,
                                            &rules,
                                            &custom_headers,
                                            &my_email_regex,
                                            cache.as_ref(),
                                        ) {
                                            log_warn!(
                                                "[Rules] queued mutation failed for mailbox '{}' op={:?}: {}",
                                                mailbox_name,
                                                op,
                                                e
                                            );
                                        }
                                    }
                                    if !removed_ids.is_empty() {
                                        log_info!(
                                            "[Rules] Filtering {} moved/deleted email(s) from response",
                                            removed_ids.len()
                                        );
                                        emails.retain(|e| !removed_ids.contains(&e.id));
                                    }
                                }
                            }

                            // Mark all fetched emails as processed
                            if let Some(ref cache) = cache {
                                let all_ids: Vec<String> =
                                    emails.iter().map(|e| e.id.clone()).collect();
                                cache.mark_rules_processed(&all_ids);
                            }
                        } else {
                            log_debug!(
                                "[Rules] Skipping auto-run for mailbox '{}' (does not match regex '{}')",
                                mailbox_name,
                                rules_mailbox_regex.as_str()
                            );
                        }
                    }

                    // Build thread counts map (unread, total) scoped to current mailbox
                    let mut thread_counts = HashMap::new();
                    let thread_ids: Vec<String> =
                        emails.iter().filter_map(|e| e.thread_id.clone()).collect();
                    if !thread_ids.is_empty() {
                        if let Ok(threads) = client.get_threads(&thread_ids) {
                            let all_email_ids: Vec<String> =
                                threads.iter().flat_map(|t| t.email_ids.clone()).collect();
                            let keyword_emails = client
                                .get_email_keywords(&all_email_ids)
                                .unwrap_or_default();
                            let email_info: HashMap<String, (bool, bool)> = keyword_emails
                                .iter()
                                .map(|e| {
                                    let seen = e.keywords.contains_key("$seen");
                                    let in_mailbox = e.mailbox_ids.contains_key(&mailbox_id);
                                    (e.id.clone(), (seen, in_mailbox))
                                })
                                .collect();
                            for thread in threads {
                                let in_folder: Vec<&String> = thread
                                    .email_ids
                                    .iter()
                                    .filter(|id| {
                                        email_info
                                            .get(*id)
                                            .map(|(_, in_mb)| *in_mb)
                                            .unwrap_or(false)
                                    })
                                    .collect();
                                let total_count = in_folder.len();
                                let unread_count = in_folder
                                    .iter()
                                    .filter(|id| {
                                        !email_info.get(**id).map(|(seen, _)| *seen).unwrap_or(true)
                                    })
                                    .count();
                                thread_counts
                                    .insert(thread.id.clone(), (unread_count, total_count));
                            }
                        }
                    }

                    Ok((emails, total, position, loaded, thread_counts))
                })();

                let (emails, total, position, loaded, thread_counts) = match result {
                    Ok((emails, total, position, loaded, thread_counts)) => {
                        (Ok(emails), total, position, loaded, thread_counts)
                    }
                    Err(e) => (Err(e), None, position, 0, HashMap::new()),
                };

                let _ = resp_tx.send(BackendResponse::Emails {
                    mailbox_id,
                    emails,
                    total,
                    position,
                    loaded,
                    thread_counts,
                });
            }
            BackendCommand::QueryThreadEmails { thread_id } => {
                let result = client
                    .query_thread_emails(&thread_id)
                    .map_err(|e| e.to_string());
                let _ = resp_tx.send(BackendResponse::ThreadEmails {
                    thread_id,
                    emails: result,
                });
            }
            BackendCommand::GetEmail { id } => {
                // Check cache first
                let cached_email = cache.as_ref().and_then(|c| c.get_email(&id));
                let result = if let Some(email) = cached_email {
                    log_debug!("[Backend] Cache hit for email {}", id);
                    Ok(email)
                } else {
                    let r = client
                        .get_email(&id)
                        .map_err(|e| e.to_string())
                        .and_then(|opt| opt.ok_or_else(|| "Email not found".to_string()));
                    if let Ok(ref email) = r {
                        if let Some(ref cache) = cache {
                            cache.put_emails(std::slice::from_ref(email));
                        }
                    }
                    r
                };

                let _ = resp_tx.send(BackendResponse::EmailBody {
                    id,
                    result: Box::new(result),
                });
            }
            BackendCommand::GetEmailForReply { id } => {
                let result = client
                    .get_email_for_reply(&id)
                    .map_err(|e| e.to_string())
                    .and_then(|opt| opt.ok_or_else(|| "Email not found".to_string()));

                let _ = resp_tx.send(BackendResponse::EmailForReply {
                    id,
                    result: Box::new(result),
                });
            }
            BackendCommand::MarkEmailRead { op_id, id } => {
                let op = QueuedMutation::MarkRead {
                    op_id,
                    id: id.clone(),
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    log_warn!("Failed to mark email {} as read: {}", id, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id,
                    action: EmailMutationAction::MarkRead,
                    result,
                });
            }
            BackendCommand::MarkEmailUnread { op_id, id } => {
                let op = QueuedMutation::MarkUnread {
                    op_id,
                    id: id.clone(),
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    let msg = msg.to_string();
                    log_warn!("Failed to mark email {} as unread: {}", id, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id,
                    action: EmailMutationAction::MarkUnread,
                    result,
                });
            }
            BackendCommand::SetEmailFlagged { op_id, id, flagged } => {
                let op = QueuedMutation::SetFlagged {
                    op_id,
                    id: id.clone(),
                    flagged,
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    let msg = msg.to_string();
                    log_warn!("Failed to set email {} flagged={}: {}", id, flagged, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id,
                    action: EmailMutationAction::SetFlagged(flagged),
                    result,
                });
            }
            BackendCommand::MoveEmail {
                op_id,
                id,
                to_mailbox_id,
            } => {
                let op = QueuedMutation::MoveEmail {
                    op_id,
                    id: id.clone(),
                    to_mailbox_id: to_mailbox_id.clone(),
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    let msg = msg.to_string();
                    log_warn!("Failed to move email {}: {}", id, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id,
                    action: EmailMutationAction::Move,
                    result,
                });
            }
            BackendCommand::MoveThread {
                op_id,
                thread_id,
                to_mailbox_id,
            } => {
                let op = QueuedMutation::MoveThread {
                    op_id,
                    thread_id: thread_id.clone(),
                    to_mailbox_id,
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    log_warn!("Failed to move thread {}: {}", thread_id, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id: thread_id,
                    action: EmailMutationAction::Move,
                    result,
                });
            }
            BackendCommand::DestroyEmail { op_id, id } => {
                let op = QueuedMutation::DestroyEmail {
                    op_id,
                    id: id.clone(),
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    let msg = msg.to_string();
                    log_warn!("Failed to destroy email {}: {}", id, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id,
                    action: EmailMutationAction::Destroy,
                    result,
                });
            }
            BackendCommand::DestroyThread { op_id, thread_id } => {
                let op = QueuedMutation::DestroyThread {
                    op_id,
                    thread_id: thread_id.clone(),
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                )
                .map_err(|msg| {
                    log_warn!("Failed to destroy thread {}: {}", thread_id, msg);
                    msg
                });
                let _ = resp_tx.send(BackendResponse::EmailMutation {
                    op_id,
                    id: thread_id,
                    action: EmailMutationAction::Destroy,
                    result,
                });
            }
            BackendCommand::MarkThreadRead {
                thread_id,
                email_ids,
            } => {
                let op = QueuedMutation::MarkThreadRead {
                    thread_id: thread_id.clone(),
                    email_ids,
                };
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                );
                let _ = resp_tx.send(BackendResponse::ThreadMarkedRead { thread_id, result });
            }
            BackendCommand::MarkMailboxRead {
                mailbox_id,
                mailbox_name,
            } => {
                log_info!(
                    "[Backend] cmd#{} MarkMailboxRead mailbox='{}' mailbox_id='{}'",
                    command_seq,
                    mailbox_name,
                    mailbox_id
                );
                let op = QueuedMutation::MarkMailboxRead {
                    mailbox_id: mailbox_id.clone(),
                    mailbox_name: mailbox_name.clone(),
                };
                let updated = fetch_all_mailbox_email_ids(client, &mailbox_id)
                    .map(|ids| ids.len())
                    .unwrap_or(0);
                let result = process_mutation_via_queue(
                    client,
                    &op,
                    &mut cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    cache.as_ref(),
                );
                let _ = resp_tx.send(BackendResponse::MailboxMarkedRead {
                    mailbox_id,
                    mailbox_name,
                    updated,
                    result,
                });
            }
            BackendCommand::GetEmailRawHeaders { id } => {
                let result = client
                    .get_email_raw(&id)
                    .map_err(|e| e.to_string())
                    .and_then(|opt| opt.ok_or_else(|| "Email not found".to_string()))
                    .map(|raw| {
                        // Extract just the headers (everything before the first blank line)
                        if let Some(pos) = raw.find("\r\n\r\n") {
                            raw[..pos].to_string()
                        } else if let Some(pos) = raw.find("\n\n") {
                            raw[..pos].to_string()
                        } else {
                            raw
                        }
                    });
                let _ = resp_tx.send(BackendResponse::EmailRawHeaders { id, result });
            }
            BackendCommand::GetEmailRaw { id } => {
                let result = client
                    .get_email_raw(&id)
                    .map_err(|e| e.to_string())
                    .and_then(|opt| opt.ok_or_else(|| "Email not found".to_string()));
                let _ = resp_tx.send(BackendResponse::EmailRaw { id, result });
            }
            BackendCommand::DownloadAttachment {
                blob_id,
                name,
                content_type,
            } => {
                let result = (|| {
                    let bytes = client
                        .download_blob(&blob_id, &name, &content_type)
                        .map_err(|e| e.to_string())?;

                    let dir = std::env::var("XDG_DOWNLOAD_DIR")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|_| {
                            std::env::var("HOME")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(|_| std::env::temp_dir())
                                .join("Downloads")
                        });
                    std::fs::create_dir_all(&dir)
                        .map_err(|e| format!("Failed to create download dir: {}", e))?;

                    let safe_name = name.replace(['/', '\\', '\0'], "_");
                    let path = dir.join(&safe_name);
                    std::fs::write(&path, &bytes)
                        .map_err(|e| format!("Failed to write file: {}", e))?;

                    log_info!(
                        "[Backend] Attachment saved: {} ({} bytes)",
                        path.display(),
                        bytes.len()
                    );
                    Ok(path)
                })();

                let _ = resp_tx.send(BackendResponse::AttachmentDownloaded { name, result });
            }
            BackendCommand::PreviewRetentionExpiry { policies } => {
                let result = collect_retention_candidates(client, &cached_mailboxes, &policies)
                    .map(|candidates| RetentionPreviewResult { candidates });
                let _ = resp_tx.send(BackendResponse::RetentionPreview { result });
            }
            BackendCommand::ExecuteRetentionExpiry { policies } => {
                let result = (|| {
                    let candidates =
                        collect_retention_candidates(client, &cached_mailboxes, &policies)?;
                    let ops = queued_mutations_for_retention(&candidates);
                    let mut deleted = 0usize;
                    let mut failed_batches = Vec::new();
                    for op in &ops {
                        match process_mutation_via_queue(
                            client,
                            op,
                            &mut cached_mailboxes,
                            &rules,
                            &custom_headers,
                            &my_email_regex,
                            cache.as_ref(),
                        ) {
                            Ok(()) => {
                                deleted += 1;
                            }
                            Err(e) => {
                                failed_batches.push(e);
                            }
                        }
                    }
                    Ok(RetentionExecutionResult {
                        deleted,
                        failed_batches,
                    })
                })();
                let _ = resp_tx.send(BackendResponse::RetentionExecuted { result });
            }
            BackendCommand::PreviewRulesForMailbox {
                origin,
                mailbox_id,
                mailbox_name,
            } => {
                log_info!(
                    "[Backend] cmd#{} PreviewRulesForMailbox origin='{}' mailbox='{}' (full mailbox scan)",
                    command_seq,
                    origin,
                    mailbox_name
                );
                let scorer = SpamScorer {
                    config: &spam_config,
                    model: &spam_model,
                };
                let result = preview_rules_for_mailbox(
                    client,
                    &cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    &mailbox_id,
                    Some(&scorer),
                );
                let _ = resp_tx.send(BackendResponse::RulesDryRun {
                    mailbox_id,
                    mailbox_name,
                    result,
                });
            }
            BackendCommand::RunRulesForMailbox {
                origin,
                mailbox_id,
                mailbox_name,
            } => {
                log_info!(
                    "[Backend] cmd#{} RunRulesForMailbox origin='{}' mailbox='{}' (full mailbox scan)",
                    command_seq,
                    origin,
                    mailbox_name
                );
                let scorer = SpamScorer {
                    config: &spam_config,
                    model: &spam_model,
                };
                let result = run_rules_for_mailbox(
                    client,
                    &cached_mailboxes,
                    &rules,
                    &custom_headers,
                    &my_email_regex,
                    &mailbox_id,
                    cache.as_ref(),
                    Some(&scorer),
                );
                let _ = resp_tx.send(BackendResponse::RulesRun {
                    mailbox_id,
                    mailbox_name,
                    result,
                });
            }
            BackendCommand::TrainMessage {
                origin,
                id,
                spam: is_spam,
            } => {
                log_info!(
                    "[Backend] cmd#{} TrainMessage origin='{}' id={} spam={}",
                    command_seq,
                    origin,
                    id,
                    is_spam
                );
                let label = if is_spam {
                    spam::Label::Spam
                } else {
                    spam::Label::Ham
                };
                let result = train_message(client, &mut spam_model, &id, label);
                match &result {
                    Ok(()) => log_info!(
                        "[Spam] trained {} as {} (model now {} spam / {} ham)",
                        id,
                        if is_spam { "spam" } else { "ham" },
                        spam_model.spam_messages(),
                        spam_model.ham_messages()
                    ),
                    Err(e) => log_warn!("[Spam] training failed for {}: {}", id, e),
                }
                let _ = resp_tx.send(BackendResponse::MessageTrained {
                    spam: is_spam,
                    result,
                });
            }
            BackendCommand::TrainMailbox {
                origin,
                mailbox_id,
                spam: is_spam,
                limit,
            } => {
                log_info!(
                    "[Backend] cmd#{} TrainMailbox origin='{}' mailbox={} spam={} limit={}",
                    command_seq,
                    origin,
                    mailbox_id,
                    is_spam,
                    limit
                );
                let label = if is_spam {
                    spam::Label::Spam
                } else {
                    spam::Label::Ham
                };
                let result = train_mailbox(client, &mut spam_model, &mailbox_id, label, limit);
                if let Ok((trained, failed)) = &result {
                    log_info!(
                        "[Spam] bulk-trained mailbox {}: {} as {}, {} failed (model now {} spam / {} ham)",
                        mailbox_id,
                        trained,
                        if is_spam { "spam" } else { "ham" },
                        failed,
                        spam_model.spam_messages(),
                        spam_model.ham_messages()
                    );
                }
                let _ = resp_tx.send(BackendResponse::MailboxTrained {
                    spam: is_spam,
                    result,
                });
            }
            BackendCommand::ClassifyMailbox {
                origin,
                mailbox_id,
                limit,
            } => {
                log_info!(
                    "[Backend] cmd#{} ClassifyMailbox origin='{}' mailbox={} limit={}",
                    command_seq,
                    origin,
                    mailbox_id,
                    limit
                );
                let result =
                    classify_mailbox(client, &spam_model, &spam_config, &mailbox_id, limit);
                let _ = resp_tx.send(BackendResponse::MailboxClassified { result });
            }
            BackendCommand::ClassifyMessage { origin, id } => {
                log_info!(
                    "[Backend] cmd#{} ClassifyMessage origin='{}' id={}",
                    command_seq,
                    origin,
                    id
                );
                let result = classify_message(client, &spam_model, &spam_config, &id);
                let _ = resp_tx.send(BackendResponse::MessageClassified { id, result });
            }
            BackendCommand::Shutdown => {
                break;
            }
        }
    }
}

/// Fetch a message's raw source, train the model with the given label, and
/// persist the updated model to disk.
fn train_message(
    client: &JmapClient,
    model: &mut SpamModel,
    id: &str,
    label: spam::Label,
) -> Result<(), String> {
    let raw = client
        .get_email_raw(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no raw message source available".to_string())?;
    let msg = spam::RawMessage::from_bytes(raw.as_bytes());
    model.train(&msg, label);
    model.save(&spam::model_path())
}

/// Bulk-train every message in a mailbox (up to `limit`, 0 = all) with one
/// label, saving the model once at the end. Returns (trained, failed) counts.
fn train_mailbox(
    client: &JmapClient,
    model: &mut SpamModel,
    mailbox_id: &str,
    label: spam::Label,
    limit: u32,
) -> Result<(usize, usize), String> {
    let mut ids = fetch_all_mailbox_email_ids(client, mailbox_id)?;
    if limit > 0 {
        ids.truncate(limit as usize);
    }
    let mut trained = 0usize;
    let mut failed = 0usize;
    for id in &ids {
        match client.get_email_raw(id) {
            Ok(Some(raw)) => {
                let msg = spam::RawMessage::from_bytes(raw.as_bytes());
                model.train(&msg, label);
                trained += 1;
            }
            Ok(None) => {
                log_debug!("[Spam] no raw source for {}; skipping", id);
                failed += 1;
            }
            Err(e) => {
                log_warn!("[Spam] failed to fetch raw {} for training: {}", id, e);
                failed += 1;
            }
        }
    }
    model.save(&spam::model_path())?;
    Ok((trained, failed))
}

/// Score a single message read-only. Returns (score, verdict).
fn classify_message(
    client: &JmapClient,
    model: &SpamModel,
    config: &SpamConfig,
    id: &str,
) -> Result<(f64, String), String> {
    let raw = client
        .get_email_raw(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no raw message source available".to_string())?;
    let msg = spam::RawMessage::from_bytes(raw.as_bytes());
    let (score, verdict) = model.classify(
        &msg,
        config.threshold,
        config.ham_threshold,
        config.min_training,
    );
    Ok((score, verdict.as_str().to_string()))
}

/// Score a mailbox sample read-only (no mutations), for validating the model.
fn classify_mailbox(
    client: &JmapClient,
    model: &SpamModel,
    config: &SpamConfig,
    mailbox_id: &str,
    limit: u32,
) -> Result<Vec<ClassifyEntry>, String> {
    let mut ids = fetch_all_mailbox_email_ids(client, mailbox_id)?;
    if limit > 0 {
        ids.truncate(limit as usize);
    }
    let emails = fetch_emails_chunked(client, &ids, &[])?;
    let mut out = Vec::with_capacity(emails.len());
    for email in &emails {
        let raw = match client.get_email_raw(&email.id) {
            Ok(Some(raw)) => raw,
            Ok(None) => continue,
            Err(e) => {
                log_warn!("[Spam] classify: failed to fetch raw {}: {}", email.id, e);
                continue;
            }
        };
        let msg = spam::RawMessage::from_bytes(raw.as_bytes());
        let (score, verdict) = model.classify(
            &msg,
            config.threshold,
            config.ham_threshold,
            config.min_training,
        );
        let from = email
            .from
            .as_ref()
            .and_then(|f| f.first())
            .and_then(|a| a.email.clone())
            .unwrap_or_default();
        out.push(ClassifyEntry {
            id: email.id.clone(),
            from,
            subject: email.subject.clone().unwrap_or_default(),
            score,
            verdict: verdict.as_str().to_string(),
        });
    }
    Ok(out)
}

const EMAIL_GET_CHUNK_SIZE: usize = 50;

fn fetch_emails_chunked(
    client: &JmapClient,
    ids: &[String],
    custom_headers: &[String],
) -> Result<Vec<Email>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(EMAIL_GET_CHUNK_SIZE) {
        let mut batch = if custom_headers.is_empty() {
            client.get_emails(chunk)
        } else {
            client.get_emails_with_extra_properties(chunk, custom_headers)
        }
        .map_err(|e| e.to_string())?;
        out.append(&mut batch);
    }
    Ok(out)
}

fn fetch_rule_emails_chunked(
    client: &JmapClient,
    ids: &[String],
    custom_headers: &[String],
) -> Result<Vec<Email>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(EMAIL_GET_CHUNK_SIZE) {
        let mut batch = client
            .get_emails_for_rules(chunk, custom_headers)
            .map_err(|e| e.to_string())?;
        out.append(&mut batch);
    }
    Ok(out)
}

/// Read-only bundle used to score messages during rule evaluation.
struct SpamScorer<'a> {
    config: &'a SpamConfig,
    model: &'a SpamModel,
}

fn mailbox_is_inbox(mailboxes: &[Mailbox], mailbox_id: &str) -> bool {
    mailboxes
        .iter()
        .any(|m| m.id == mailbox_id && m.role.as_deref() == Some("inbox"))
}

/// Score new INBOX messages and inject synthetic `X-Tmc-Spam-Score` /
/// `X-Tmc-Spam-Verdict` headers into `email.extra` so the rules engine can act
/// on them via rules.toml. The classifier itself never moves or deletes mail.
///
/// No-op unless: a scorer is present and enabled, the model is trained past its
/// cold-start gate, and `mailbox_id` is the INBOX. Skipping while untrained
/// avoids fetching raw message bodies before the filter can do anything useful.
fn annotate_inbox_spam(
    client: &JmapClient,
    scorer: Option<&SpamScorer>,
    mailboxes: &[Mailbox],
    mailbox_id: &str,
    emails: &mut [Email],
) {
    let Some(scorer) = scorer else {
        return;
    };
    if !scorer.config.enabled || !scorer.model.is_trained(scorer.config.min_training) {
        return;
    }
    if !mailbox_is_inbox(mailboxes, mailbox_id) {
        return;
    }

    log_info!(
        "[Spam] scoring {} INBOX message(s) in mailbox {}",
        emails.len(),
        mailbox_id
    );
    for email in emails.iter_mut() {
        let raw = match client.get_email_raw(&email.id) {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                log_debug!("[Spam] no raw source for email {}; skipping", email.id);
                continue;
            }
            Err(e) => {
                log_warn!("[Spam] failed to fetch raw email {}: {}", email.id, e);
                continue;
            }
        };
        let msg = spam::RawMessage::from_bytes(raw.as_bytes());
        let (score, verdict) = scorer.model.classify(
            &msg,
            scorer.config.threshold,
            scorer.config.ham_threshold,
            scorer.config.min_training,
        );
        email.extra.insert(
            "header:X-Tmc-Spam-Score:asText".to_string(),
            Json::Str(format!("{:.4}", score)),
        );
        email.extra.insert(
            "header:X-Tmc-Spam-Verdict:asText".to_string(),
            Json::Str(verdict.as_str().to_string()),
        );
        log_info!(
            "[Spam] email {} score={:.4} verdict={}",
            email.id,
            score,
            verdict.as_str()
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn run_rules_for_mailbox(
    client: &JmapClient,
    mailboxes: &[Mailbox],
    rules: &[CompiledRule],
    custom_headers: &[String],
    my_email_regex: &UserRegex,
    mailbox_id: &str,
    cache: Option<&Cache>,
    spam: Option<&SpamScorer>,
) -> Result<RulesRunResult, String> {
    if rules.is_empty() {
        return Ok(RulesRunResult {
            scanned: 0,
            matched_rules: 0,
            actions: 0,
        });
    }

    let ids = fetch_all_mailbox_email_ids(client, mailbox_id)?;
    if ids.is_empty() {
        return Ok(RulesRunResult {
            scanned: 0,
            matched_rules: 0,
            actions: 0,
        });
    }

    let mut emails = fetch_rule_emails_chunked(client, &ids, custom_headers)?;
    let scanned = emails.len();
    annotate_inbox_spam(client, spam, mailboxes, mailbox_id, &mut emails);
    let applications = rules::apply_rules(rules, &emails, mailboxes, my_email_regex);
    let matched_rules = applications.len();
    let actions = applications.iter().map(|a| a.actions.len()).sum::<usize>();
    if !applications.is_empty() {
        let mut mailboxes_for_mutation = mailboxes.to_vec();
        let ops = queued_mutations_for_rule_actions(&applications, mailboxes);
        for op in &ops {
            process_mutation_via_queue(
                client,
                op,
                &mut mailboxes_for_mutation,
                rules,
                custom_headers,
                my_email_regex,
                cache,
            )?;
        }
    }

    // Mark all scanned emails as processed (explicit run bypasses processed check
    // but still marks them so future auto-runs skip them)
    if let Some(cache) = cache {
        let all_ids: Vec<String> = emails.iter().map(|e| e.id.clone()).collect();
        cache.mark_rules_processed(&all_ids);
    }

    Ok(RulesRunResult {
        scanned,
        matched_rules,
        actions,
    })
}

fn preview_rules_for_mailbox(
    client: &JmapClient,
    mailboxes: &[Mailbox],
    rules: &[CompiledRule],
    custom_headers: &[String],
    my_email_regex: &UserRegex,
    mailbox_id: &str,
    spam: Option<&SpamScorer>,
) -> Result<RulesDryRunResult, String> {
    if rules.is_empty() {
        return Ok(RulesDryRunResult {
            scanned: 0,
            matched_rules: 0,
            actions: 0,
            entries: Vec::new(),
        });
    }

    let ids = fetch_all_mailbox_email_ids(client, mailbox_id)?;
    if ids.is_empty() {
        return Ok(RulesDryRunResult {
            scanned: 0,
            matched_rules: 0,
            actions: 0,
            entries: Vec::new(),
        });
    }

    let mut emails = fetch_rule_emails_chunked(client, &ids, custom_headers)?;
    let scanned = emails.len();
    annotate_inbox_spam(client, spam, mailboxes, mailbox_id, &mut emails);
    let mut entries = Vec::new();
    let email_by_id: HashMap<String, &Email> = emails.iter().map(|e| (e.id.clone(), e)).collect();
    let applications = rules::apply_rules(rules, &emails, mailboxes, my_email_regex);
    let matched_rules = applications.len();
    let actions = applications.iter().map(|a| a.actions.len()).sum::<usize>();

    for app in applications {
        if let Some(email) = email_by_id.get(&app.email_id) {
            let from = email
                .from
                .as_ref()
                .and_then(|f| f.first())
                .map(|a| a.to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            let received_at = email
                .received_at
                .as_deref()
                .map(|d| d.chars().take(10).collect::<String>())
                .unwrap_or_else(|| "(unknown)".to_string());
            let subject = email
                .subject
                .clone()
                .unwrap_or_else(|| "(no subject)".to_string());
            let action_names = app.actions.iter().map(format_rule_action).collect();
            entries.push(RulesDryRunEntry {
                received_at,
                from,
                subject,
                rule_name: app.rule_name,
                actions: action_names,
            });
        }
    }

    Ok(RulesDryRunResult {
        scanned,
        matched_rules,
        actions,
        entries,
    })
}

const MAILBOX_QUERY_CHUNK_SIZE: u32 = 500;

fn fetch_all_mailbox_email_ids(
    client: &JmapClient,
    mailbox_id: &str,
) -> Result<Vec<String>, String> {
    let mut seen_ids = HashSet::new();
    let mut ids = Vec::new();
    let mut position = 0u32;

    loop {
        let query = client
            .query_emails_uncollapsed(mailbox_id, MAILBOX_QUERY_CHUNK_SIZE, position)
            .map_err(|e| e.to_string())?;
        if query.ids.is_empty() {
            break;
        }

        let loaded = query.ids.len() as u32;
        for id in query.ids {
            if seen_ids.insert(id.clone()) {
                ids.push(id);
            }
        }

        position = query.position.saturating_add(loaded);
        if loaded == 0 {
            break;
        }
        if let Some(total) = query.total {
            if position >= total {
                break;
            }
        }
    }

    Ok(ids)
}

fn format_rule_action(action: &rules::Action) -> String {
    match action {
        rules::Action::MarkRead => "mark_read".to_string(),
        rules::Action::MarkUnread => "mark_unread".to_string(),
        rules::Action::Flag => "flag".to_string(),
        rules::Action::Unflag => "unflag".to_string(),
        rules::Action::Move { target } => format!("move_to={}", target),
        rules::Action::Delete => "delete".to_string(),
    }
}

fn next_generated_op_id() -> u64 {
    GENERATED_OP_ID.fetch_add(1, Ordering::Relaxed)
}

fn queued_mutations_for_rule_actions(
    applications: &[rules::RuleApplication],
    mailboxes: &[Mailbox],
) -> Vec<QueuedMutation> {
    let trash_id = mailboxes
        .iter()
        .find(|m| m.role.as_deref() == Some("trash"))
        .map(|m| m.id.clone());
    let mut out = Vec::new();

    for app in applications {
        for action in &app.actions {
            let op_id = next_generated_op_id();
            let queued =
                match action {
                    rules::Action::MarkRead => Some(QueuedMutation::MarkRead {
                        op_id,
                        id: app.email_id.clone(),
                    }),
                    rules::Action::MarkUnread => Some(QueuedMutation::MarkUnread {
                        op_id,
                        id: app.email_id.clone(),
                    }),
                    rules::Action::Flag => Some(QueuedMutation::SetFlagged {
                        op_id,
                        id: app.email_id.clone(),
                        flagged: true,
                    }),
                    rules::Action::Unflag => Some(QueuedMutation::SetFlagged {
                        op_id,
                        id: app.email_id.clone(),
                        flagged: false,
                    }),
                    rules::Action::Move { target } => rules::resolve_mailbox_id(target, mailboxes)
                        .map(|to_mailbox_id| QueuedMutation::MoveEmail {
                            op_id,
                            id: app.email_id.clone(),
                            to_mailbox_id,
                        }),
                    rules::Action::Delete => {
                        trash_id
                            .as_ref()
                            .map(|to_mailbox_id| QueuedMutation::MoveEmail {
                                op_id,
                                id: app.email_id.clone(),
                                to_mailbox_id: to_mailbox_id.clone(),
                            })
                    }
                };
            if let Some(queued) = queued {
                out.push(queued);
            }
        }
    }
    out
}

fn queued_mutations_for_retention(candidates: &[RetentionCandidate]) -> Vec<QueuedMutation> {
    candidates
        .iter()
        .map(|c| QueuedMutation::DestroyEmail {
            op_id: next_generated_op_id(),
            id: c.id.clone(),
        })
        .collect()
}

fn collect_retention_candidates(
    client: &JmapClient,
    mailboxes: &[Mailbox],
    policies: &[RetentionPolicyConfig],
) -> Result<Vec<RetentionCandidate>, String> {
    if policies.is_empty() {
        return Ok(Vec::new());
    }

    let today_days = current_days_since_epoch()?;
    let mut candidates = Vec::new();
    let mut seen_email_ids = HashSet::new();

    for policy in policies {
        let Some(mailbox_id) = rules::resolve_mailbox_id(&policy.folder, mailboxes) else {
            log_warn!(
                "Retention policy '{}' skipped; cannot resolve folder '{}'",
                policy.name,
                policy.folder
            );
            continue;
        };
        let mailbox_name = mailboxes
            .iter()
            .find(|m| m.id == mailbox_id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| policy.folder.clone());
        let cutoff_days = today_days - (policy.days as i64);

        let mut position = 0u32;
        loop {
            let query = client
                .query_emails_uncollapsed(&mailbox_id, 500, position)
                .map_err(|e| e.to_string())?;
            if query.ids.is_empty() {
                break;
            }

            let emails = fetch_emails_chunked(client, &query.ids, &[])?;
            for email in emails {
                if !seen_email_ids.insert(email.id.clone()) {
                    continue;
                }
                let Some(received_days) = email_received_days(&email) else {
                    continue;
                };
                if received_days >= cutoff_days {
                    continue;
                }

                let from = email
                    .from
                    .as_ref()
                    .and_then(|f| f.first())
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "(unknown)".to_string());
                let received_at = email
                    .received_at
                    .as_deref()
                    .map(|d| d.chars().take(10).collect::<String>())
                    .unwrap_or_else(|| "(unknown)".to_string());

                candidates.push(RetentionCandidate {
                    id: email.id,
                    mailbox: mailbox_name.clone(),
                    policy: policy.name.clone(),
                    received_at,
                    from,
                    subject: email.subject.unwrap_or_else(|| "(no subject)".to_string()),
                });
            }

            let loaded = query.ids.len() as u32;
            position = query.position.saturating_add(loaded);
            if loaded == 0 {
                break;
            }
            if let Some(total) = query.total {
                if position >= total {
                    break;
                }
            }
        }
    }

    candidates.sort_by(|a, b| {
        a.received_at
            .cmp(&b.received_at)
            .then_with(|| a.mailbox.cmp(&b.mailbox))
            .then_with(|| a.subject.cmp(&b.subject))
    });
    Ok(candidates)
}

fn current_days_since_epoch() -> Result<i64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {}", e))?;
    Ok((now.as_secs() / 86_400) as i64)
}

fn email_received_days(email: &Email) -> Option<i64> {
    let received = email.received_at.as_deref()?;
    let y = received.get(0..4)?.parse::<i32>().ok()?;
    let m = received.get(5..7)?.parse::<u32>().ok()?;
    let d = received.get(8..10)?.parse::<u32>().ok()?;
    ymd_to_days_since_epoch(y, m, d)
}

// Convert calendar date to day index since Unix epoch.
fn ymd_to_days_since_epoch(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month as i64 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use std::collections::HashMap;

    fn make_email(id: &str) -> Email {
        Email {
            id: id.to_string(),
            thread_id: Some("thread-1".to_string()),
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(format!("Subject {}", id)),
            received_at: Some("2025-01-01T00:00:00Z".to_string()),
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn queued_rule_actions_compile_to_email_mutations() {
        let mailboxes = vec![
            Mailbox {
                id: "inbox".to_string(),
                name: "INBOX".to_string(),
                parent_id: None,
                role: Some("inbox".to_string()),
                total_emails: 1,
                unread_emails: 1,
                sort_order: 0,
            },
            Mailbox {
                id: "archive".to_string(),
                name: "Archive".to_string(),
                parent_id: None,
                role: Some("archive".to_string()),
                total_emails: 0,
                unread_emails: 0,
                sort_order: 1,
            },
            Mailbox {
                id: "trash".to_string(),
                name: "Trash".to_string(),
                parent_id: None,
                role: Some("trash".to_string()),
                total_emails: 0,
                unread_emails: 0,
                sort_order: 2,
            },
        ];
        let apps = vec![rules::RuleApplication {
            email_id: "e1".to_string(),
            rule_name: "r1".to_string(),
            actions: vec![
                rules::Action::MarkRead,
                rules::Action::Flag,
                rules::Action::Move {
                    target: "Archive".to_string(),
                },
                rules::Action::Delete,
            ],
        }];

        let ops = queued_mutations_for_rule_actions(&apps, &mailboxes);
        assert_eq!(ops.len(), 4);
        assert!(ops
            .iter()
            .any(|op| matches!(op, QueuedMutation::MarkRead { id, .. } if id == "e1")));
        assert!(ops.iter().any(
            |op| matches!(op, QueuedMutation::SetFlagged { id, flagged, .. } if id == "e1" && *flagged)
        ));
        let move_targets: Vec<String> = ops
            .iter()
            .filter_map(|op| match op {
                QueuedMutation::MoveEmail {
                    id, to_mailbox_id, ..
                } if id == "e1" => Some(to_mailbox_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(move_targets.len(), 2);
        assert!(move_targets.contains(&"archive".to_string()));
        assert!(move_targets.contains(&"trash".to_string()));
    }

    #[test]
    fn apply_local_mark_thread_read_updates_seen_and_unread_counts() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("backend_thread_mark_read").unwrap();

        let mut e1 = make_email("e1");
        e1.mailbox_ids.insert("inbox".to_string(), true);
        let mut e2 = make_email("e2");
        e2.mailbox_ids.insert("inbox".to_string(), true);
        cache.put_emails(&[e1.clone(), e2.clone()]);
        cache.put_mailbox_index("inbox", &["e1".into(), "e2".into()]);
        cache.put_mailboxes(&[Mailbox {
            id: "inbox".to_string(),
            name: "INBOX".to_string(),
            parent_id: None,
            role: Some("inbox".to_string()),
            total_emails: 2,
            unread_emails: 2,
            sort_order: 0,
        }]);

        let op = QueuedMutation::MarkThreadRead {
            thread_id: "thread-1".to_string(),
            email_ids: vec!["e1".to_string(), "e2".to_string()],
        };
        apply_local_mutation(Some(&cache), &op);

        assert!(cache
            .get_email("e1")
            .unwrap()
            .keywords
            .contains_key("$seen"));
        assert!(cache
            .get_email("e2")
            .unwrap()
            .keywords
            .contains_key("$seen"));
        assert_eq!(cache.get_mailboxes().unwrap()[0].unread_emails, 0);
    }

    #[test]
    fn queued_retention_actions_map_to_destroy_email_ops() {
        let candidates = vec![
            RetentionCandidate {
                id: "e1".to_string(),
                mailbox: "Trash".to_string(),
                policy: "p".to_string(),
                received_at: "2024-01-01".to_string(),
                from: "a@example.com".to_string(),
                subject: "s1".to_string(),
            },
            RetentionCandidate {
                id: "e2".to_string(),
                mailbox: "Trash".to_string(),
                policy: "p".to_string(),
                received_at: "2024-01-02".to_string(),
                from: "b@example.com".to_string(),
                subject: "s2".to_string(),
            },
        ];
        let ops = queued_mutations_for_retention(&candidates);
        assert_eq!(ops.len(), 2);
        assert!(matches!(
            &ops[0],
            QueuedMutation::DestroyEmail { id, .. } if id == "e1"
        ));
        assert!(matches!(
            &ops[1],
            QueuedMutation::DestroyEmail { id, .. } if id == "e2"
        ));
    }
}
