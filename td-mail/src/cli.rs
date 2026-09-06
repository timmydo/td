use crate::backend::{self, BackendCommand, BackendResponse};
use crate::compose;
use crate::config::Config;
use crate::jmap::types::{Email, Mailbox};
use crate::json::{self, Json as Value};
use crate::keybindings;
use crate::regex::UserRegex;
use crate::rules::{self, CompiledRule};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::{mpsc, Arc};

#[derive(Clone, Copy)]
enum TriageTarget {
    Archive,
    Trash,
    Keep,
}

#[derive(Clone)]
struct TriageItem {
    id: String,
    received_at: Option<String>,
    from: Option<String>,
    subject: Option<String>,
    target: TriageTarget,
    reason: String,
    confidence: f32,
}

struct TriagePlan {
    archive_ids: Vec<String>,
    trash_ids: Vec<String>,
}

struct CliState {
    config: Config,
    cmd_tx: Option<mpsc::Sender<BackendCommand>>,
    resp_rx: Option<mpsc::Receiver<BackendResponse>>,
    connected_account: Option<String>,
    connected_username: Option<String>,
    cached_mailboxes: Vec<Mailbox>,
    next_op_id: u64,
    rules: Arc<Vec<CompiledRule>>,
    custom_headers: Arc<Vec<String>>,
    rules_mailbox_regex: Arc<UserRegex>,
    my_email_regex: Arc<UserRegex>,
    archive_folder: String,
    deleted_folder: String,
    archive_mailbox_id: Option<String>,
    deleted_mailbox_id: Option<String>,
    next_plan_id: u64,
    triage_plans: HashMap<String, TriagePlan>,
    offline: bool,
}

impl CliState {
    fn next_op_id(&mut self) -> u64 {
        self.next_op_id += 1;
        self.next_op_id
    }

    fn send_cmd(&self, cmd: BackendCommand) -> Result<(), String> {
        self.cmd_tx
            .as_ref()
            .ok_or_else(|| "not connected".to_string())?
            .send(cmd)
            .map_err(|_| "backend channel closed".to_string())
    }

    fn recv_resp(&self) -> Result<BackendResponse, String> {
        self.resp_rx
            .as_ref()
            .ok_or_else(|| "not connected".to_string())?
            .recv()
            .map_err(|_| "backend channel closed".to_string())
    }

    fn next_plan_id(&mut self) -> String {
        self.next_plan_id += 1;
        format!("plan-{}", self.next_plan_id)
    }

    fn refresh_mailboxes(&mut self, origin: &str) -> Result<(), String> {
        self.send_cmd(BackendCommand::FetchMailboxes {
            origin: origin.to_string(),
        })?;
        match self.recv_resp()? {
            BackendResponse::Mailboxes(Ok(mailboxes)) => {
                self.cached_mailboxes = mailboxes;
                Ok(())
            }
            BackendResponse::Mailboxes(Err(e)) => Err(e),
            _ => Err("unexpected response from backend".to_string()),
        }
    }

    fn resolve_folder_id(
        &mut self,
        folder_name: &str,
        preferred_id: Option<&str>,
    ) -> Option<String> {
        if let Some(id) = preferred_id {
            return Some(id.to_string());
        }
        if self.cached_mailboxes.is_empty() {
            let _ = self.refresh_mailboxes("cli:auto-cache");
        }
        if let Some(id) = rules::resolve_mailbox_id(folder_name, &self.cached_mailboxes) {
            return Some(id);
        }
        if self.refresh_mailboxes("cli:refresh-resolve").is_ok() {
            return rules::resolve_mailbox_id(folder_name, &self.cached_mailboxes);
        }
        None
    }

    fn reply_from_header(&self) -> String {
        self.config
            .mail
            .reply_from
            .clone()
            .or_else(|| self.connected_username.clone())
            .unwrap_or_else(|| "user@example.com".to_string())
    }
}

fn ok_response(data: Value) -> Value {
    let mut obj = json::ObjectBuilder::new().set("ok", true);
    match data {
        Value::Obj(pairs) => {
            for (key, value) in pairs {
                obj = obj.set(key, value);
            }
        }
        other => obj = obj.set("data", other),
    }
    // `ok` is this layer's answer, whatever the payload carried under that name.
    obj.set("ok", true).build()
}

fn err_response(msg: &str) -> Value {
    json!({"ok": false, "error": msg})
}

fn serialize_email(email: &Email, headers_only: bool, max_body_chars: usize) -> Value {
    let from = email.from.as_ref().map(|addrs| {
        addrs
            .iter()
            .map(|a| json!({"name": a.name, "email": a.email}))
            .collect::<Vec<_>>()
    });
    let to = email.to.as_ref().map(|addrs| {
        addrs
            .iter()
            .map(|a| json!({"name": a.name, "email": a.email}))
            .collect::<Vec<_>>()
    });
    let cc = email.cc.as_ref().map(|addrs| {
        addrs
            .iter()
            .map(|a| json!({"name": a.name, "email": a.email}))
            .collect::<Vec<_>>()
    });

    let is_read = email.keywords.contains_key("$seen");
    let is_flagged = email.keywords.contains_key("$flagged");

    let attachments: Option<Vec<Value>> = email.attachments.as_ref().map(|parts| {
        parts
            .iter()
            .map(|p| {
                json!({
                    "part_id": p.part_id,
                    "blob_id": p.blob_id,
                    "type": p.r#type,
                    "name": p.name,
                    "size": p.size,
                })
            })
            .collect()
    });

    let mut obj = json!({
        "id": email.id,
        "thread_id": email.thread_id,
        "from": from,
        "to": to,
        "cc": cc,
        "subject": email.subject,
        "received_at": email.received_at,
        "sent_at": email.sent_at,
        "is_read": is_read,
        "is_flagged": is_flagged,
        "mailbox_ids": email.mailbox_ids.keys().collect::<Vec<_>>(),
        "message_id": email.message_id,
        "references": email.references,
        "attachments": attachments,
    });

    if !headers_only {
        let body_text = compose::extract_body_text(email);
        let (body, truncated) = if max_body_chars > 0 && body_text.len() > max_body_chars {
            let truncated_body: String = body_text.chars().take(max_body_chars).collect();
            (truncated_body, true)
        } else {
            (body_text, false)
        };
        obj.insert("body", json!(body));
        obj.insert("body_truncated", json!(truncated));
        obj.insert("preview", json!(email.preview));
    }

    obj
}

fn serialize_mailbox(mbox: &Mailbox) -> Value {
    json!({
        "id": mbox.id,
        "name": mbox.name,
        "parent_id": mbox.parent_id,
        "role": mbox.role,
        "total_emails": mbox.total_emails,
        "unread_emails": mbox.unread_emails,
        "sort_order": mbox.sort_order,
    })
}

fn dispatch(state: &mut CliState, input: &Value) -> Value {
    let command = match input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return err_response("missing 'command' field"),
    };

    match command {
        "list_accounts" => cmd_list_accounts(state),
        "connect" => cmd_connect(state, input),
        "status" => cmd_status(state),
        "list_mailboxes" => cmd_list_mailboxes(state),
        "create_mailbox" => cmd_create_mailbox(state, input),
        "delete_mailbox" => cmd_delete_mailbox(state, input),
        "query_emails" => cmd_query_emails(state, input),
        "get_email" => cmd_get_email(state, input),
        "get_thread" => cmd_get_thread(state, input),
        "mark_read" => cmd_mark_read(state, input),
        "mark_unread" => cmd_mark_unread(state, input),
        "flag" => cmd_flag(state, input),
        "unflag" => cmd_unflag(state, input),
        "move_email" => cmd_move_email(state, input),
        "bulk_move" => cmd_bulk_move(state, input),
        "archive" => cmd_archive(state, input),
        "bulk_archive" => cmd_bulk_archive(state, input),
        "delete_email" => cmd_delete_email(state, input),
        "bulk_delete_email" => cmd_bulk_delete_email(state, input),
        "destroy" => cmd_destroy(state, input),
        "triage_suggest" => cmd_triage_suggest(state, input),
        "apply_triage_plan" => cmd_apply_triage_plan(state, input),
        "train" => cmd_train(state, input),
        "train_mailbox" => cmd_train_mailbox(state, input),
        "classify" => cmd_classify(state, input),
        "mark_mailbox_read" => cmd_mark_mailbox_read(state, input),
        "get_raw_headers" => cmd_get_raw_headers(state, input),
        "download_attachment" => cmd_download_attachment(state, input),
        "compose_draft" => cmd_compose_draft(state),
        "reply_draft" => cmd_reply_draft(state, input),
        "forward_draft" => cmd_forward_draft(state, input),
        "keybindings" => cmd_keybindings(),
        _ => err_response(&format!("unknown command '{}'", command)),
    }
}

// --- Command handlers ---

fn cmd_list_accounts(state: &CliState) -> Value {
    let accounts: Vec<Value> = state
        .config
        .accounts
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "username": a.username,
                "well_known_url": a.well_known_url,
            })
        })
        .collect();
    ok_response(json!({"accounts": accounts}))
}

fn cmd_connect(state: &mut CliState, input: &Value) -> Value {
    let account_name = match input.get("account").and_then(|v| v.as_str()) {
        Some(name) => name,
        None => return err_response("missing 'account' field"),
    };

    let account = match state
        .config
        .accounts
        .iter()
        .find(|a| a.name == account_name)
    {
        Some(a) => a.clone(),
        None => return err_response(&format!("unknown account '{}'", account_name)),
    };

    // Disconnect existing backend if any
    if let Some(ref tx) = state.cmd_tx {
        let _ = tx.send(BackendCommand::Shutdown);
    }
    state.cmd_tx = None;
    state.resp_rx = None;
    state.connected_account = None;
    state.connected_username = None;
    state.cached_mailboxes.clear();

    let client = if state.offline {
        None
    } else {
        match crate::connect_account(&account) {
            Ok(c) => Some(c),
            Err(e) => return err_response(&format!("connection failed: {}", e)),
        }
    };

    let (cmd_tx, resp_rx) = backend::spawn(
        client,
        account.name.clone(),
        state.rules.clone(),
        state.custom_headers.clone(),
        state.rules_mailbox_regex.clone(),
        state.my_email_regex.clone(),
        state.config.spam.clone(),
    );

    state.cmd_tx = Some(cmd_tx);
    state.resp_rx = Some(resp_rx);
    state.connected_account = Some(account.name.clone());
    state.connected_username = Some(account.username.clone());

    ok_response(json!({
        "account": account.name,
        "username": account.username,
        "offline": state.offline,
    }))
}

fn cmd_status(state: &CliState) -> Value {
    ok_response(json!({
        "connected": state.cmd_tx.is_some(),
        "account": state.connected_account,
        "username": state.connected_username,
        "cached_mailboxes": state.cached_mailboxes.len(),
    }))
}

fn cmd_list_mailboxes(state: &mut CliState) -> Value {
    if let Err(e) = state.send_cmd(BackendCommand::FetchMailboxes {
        origin: "cli".to_string(),
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::Mailboxes(Ok(mailboxes))) => {
            state.cached_mailboxes = mailboxes.clone();
            let list: Vec<Value> = mailboxes.iter().map(serialize_mailbox).collect();
            ok_response(json!({"mailboxes": list}))
        }
        Ok(BackendResponse::Mailboxes(Err(e))) => err_response(&e),
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_create_mailbox(state: &mut CliState, input: &Value) -> Value {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return err_response("missing 'name' field"),
    };

    if let Err(e) = state.send_cmd(BackendCommand::CreateMailbox { name: name.clone() }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::MailboxCreated { name, result }) => match result {
            Ok(()) => ok_response(json!({"name": name})),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_delete_mailbox(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'mailbox_id' field"),
    };

    let name = state
        .cached_mailboxes
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| id.clone());

    if let Err(e) = state.send_cmd(BackendCommand::DeleteMailbox {
        id,
        name: name.clone(),
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::MailboxDeleted { name, result }) => match result {
            Ok(()) => ok_response(json!({"name": name})),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_query_emails(state: &mut CliState, input: &Value) -> Value {
    let mailbox_id = match input.get("mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'mailbox_id' field"),
    };
    let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
    let position = input.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let search = input
        .get("search")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let received_after = input
        .get("received_after")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let received_before = input
        .get("received_before")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Err(e) = state.send_cmd(BackendCommand::QueryEmails {
        origin: "cli".to_string(),
        mailbox_id: mailbox_id.clone(),
        page_size: limit,
        position,
        search_query: search,
        received_after,
        received_before,
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::Emails {
            emails: Ok(emails),
            total,
            position,
            loaded,
            thread_counts,
            ..
        }) => {
            let headers_only = input
                .get("headers_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let max_body_chars = input
                .get("max_body_chars")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            let list: Vec<Value> = emails
                .iter()
                .map(|e| serialize_email(e, headers_only, max_body_chars))
                .collect();

            let mut tc = json::ObjectBuilder::with_capacity(thread_counts.len());
            for (tid, (unread, total)) in thread_counts.iter() {
                tc.insert(tid.clone(), json!({"unread": unread, "total": total}));
            }
            let tc = tc.build();

            ok_response(json!({
                "emails": list,
                "total": total,
                "position": position,
                "loaded": loaded,
                "thread_counts": tc,
            }))
        }
        Ok(BackendResponse::Emails { emails: Err(e), .. }) => err_response(&e),
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_get_email(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };

    let headers_only = input
        .get("headers_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_body_chars = input
        .get("max_body_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    if let Err(e) = state.send_cmd(BackendCommand::GetEmail { id }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::EmailBody {
            result: boxed_result,
            ..
        }) => match *boxed_result {
            Ok(email) => {
                let data = serialize_email(&email, headers_only, max_body_chars);
                ok_response(data)
            }
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_get_thread(state: &mut CliState, input: &Value) -> Value {
    let thread_id = match input.get("thread_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'thread_id' field"),
    };

    let headers_only = input
        .get("headers_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_body_chars = input
        .get("max_body_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    if let Err(e) = state.send_cmd(BackendCommand::QueryThreadEmails {
        thread_id: thread_id.clone(),
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::ThreadEmails {
            emails: Ok(emails), ..
        }) => {
            let list: Vec<Value> = emails
                .iter()
                .map(|e| serialize_email(e, headers_only, max_body_chars))
                .collect();
            ok_response(json!({
                "thread_id": thread_id,
                "emails": list,
            }))
        }
        Ok(BackendResponse::ThreadEmails { emails: Err(e), .. }) => err_response(&e),
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_mark_read(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::MarkEmailRead { op_id, id }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn cmd_mark_unread(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::MarkEmailUnread { op_id, id }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn cmd_flag(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::SetEmailFlagged {
        op_id,
        id,
        flagged: true,
    }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn cmd_unflag(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::SetEmailFlagged {
        op_id,
        id,
        flagged: false,
    }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn cmd_move_email(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let to_mailbox_id = match input.get("to_mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'to_mailbox_id' field"),
    };
    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::MoveEmail {
        op_id,
        id,
        to_mailbox_id,
    }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn parse_ids(input: &Value) -> Result<Vec<String>, String> {
    let ids = input
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing 'ids' field".to_string())?;
    let mut out = Vec::with_capacity(ids.len());
    for v in ids {
        let id = v
            .as_str()
            .ok_or_else(|| "'ids' must be an array of strings".to_string())?;
        out.push(id.to_string());
    }
    Ok(out)
}

fn mutate_many_move(state: &mut CliState, ids: &[String], target_mailbox_id: &str) -> Value {
    let mut results = Vec::with_capacity(ids.len());
    for id in ids {
        let op_id = state.next_op_id();
        let send_result = state.send_cmd(BackendCommand::MoveEmail {
            op_id,
            id: id.clone(),
            to_mailbox_id: target_mailbox_id.to_string(),
        });
        if let Err(e) = send_result {
            results.push(json!({"id": id, "ok": false, "error": e}));
            continue;
        }
        match state.recv_resp() {
            Ok(BackendResponse::EmailMutation { result, id, .. }) => match result {
                Ok(()) => results.push(json!({"id": id, "ok": true})),
                Err(e) => results.push(json!({"id": id, "ok": false, "error": e})),
            },
            Ok(_) => results.push(json!({"id": id, "ok": false, "error": "unexpected response"})),
            Err(e) => results.push(json!({"id": id, "ok": false, "error": e})),
        }
    }
    let success = results
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    ok_response(json!({
        "target_mailbox_id": target_mailbox_id,
        "attempted": ids.len(),
        "succeeded": success,
        "failed": ids.len().saturating_sub(success),
        "results": results
    }))
}

fn cmd_bulk_move(state: &mut CliState, input: &Value) -> Value {
    let ids = match parse_ids(input) {
        Ok(ids) => ids,
        Err(e) => return err_response(&e),
    };
    let to_mailbox_id = match input.get("to_mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'to_mailbox_id' field"),
    };
    mutate_many_move(state, &ids, &to_mailbox_id)
}

fn cmd_archive(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };

    let archive_folder = state.archive_folder.clone();
    let archive_mailbox_id = state.archive_mailbox_id.clone();
    let archive_id = match state.resolve_folder_id(&archive_folder, archive_mailbox_id.as_deref()) {
        Some(id) => id,
        None => {
            return err_response(&format!(
                "cannot resolve archive folder '{}'",
                archive_folder
            ))
        }
    };

    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::MoveEmail {
        op_id,
        id,
        to_mailbox_id: archive_id,
    }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn cmd_bulk_archive(state: &mut CliState, input: &Value) -> Value {
    let ids = match parse_ids(input) {
        Ok(ids) => ids,
        Err(e) => return err_response(&e),
    };
    let archive_folder = state.archive_folder.clone();
    let archive_mailbox_id = state.archive_mailbox_id.clone();
    let archive_id = match state.resolve_folder_id(&archive_folder, archive_mailbox_id.as_deref()) {
        Some(id) => id,
        None => {
            return err_response(&format!(
                "cannot resolve archive folder '{}'",
                archive_folder
            ))
        }
    };
    mutate_many_move(state, &ids, &archive_id)
}

fn cmd_delete_email(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };

    let deleted_folder = state.deleted_folder.clone();
    let deleted_mailbox_id = state.deleted_mailbox_id.clone();
    let deleted_id = match state.resolve_folder_id(&deleted_folder, deleted_mailbox_id.as_deref()) {
        Some(id) => id,
        None => {
            return err_response(&format!(
                "cannot resolve deleted folder '{}'",
                deleted_folder
            ))
        }
    };

    let op_id = state.next_op_id();

    if let Err(e) = state.send_cmd(BackendCommand::MoveEmail {
        op_id,
        id,
        to_mailbox_id: deleted_id,
    }) {
        return err_response(&e);
    }

    recv_mutation_response(state)
}

fn cmd_bulk_delete_email(state: &mut CliState, input: &Value) -> Value {
    let ids = match parse_ids(input) {
        Ok(ids) => ids,
        Err(e) => return err_response(&e),
    };
    let deleted_folder = state.deleted_folder.clone();
    let deleted_mailbox_id = state.deleted_mailbox_id.clone();
    let deleted_id = match state.resolve_folder_id(&deleted_folder, deleted_mailbox_id.as_deref()) {
        Some(id) => id,
        None => {
            return err_response(&format!(
                "cannot resolve deleted folder '{}'",
                deleted_folder
            ))
        }
    };
    mutate_many_move(state, &ids, &deleted_id)
}

fn triage_from_rules(
    state: &CliState,
    emails: &[Email],
) -> HashMap<String, (TriageTarget, String, f32)> {
    let mut out = HashMap::new();
    if state.rules.is_empty() {
        return out;
    }
    let applications = rules::apply_rules(
        &state.rules,
        emails,
        &state.cached_mailboxes,
        &state.my_email_regex,
    );
    let mut by_name = HashMap::new();
    for rule in state.rules.iter() {
        by_name.insert(rule.name.as_str(), rule);
    }
    for app in applications {
        let mut target = TriageTarget::Keep;
        if let Some(rule) = by_name.get(app.rule_name.as_str()) {
            if let Some(hint) = rule.triage_action {
                target = match hint {
                    rules::TriageHintActionDef::Archive => TriageTarget::Archive,
                    rules::TriageHintActionDef::Trash => TriageTarget::Trash,
                    rules::TriageHintActionDef::Keep => TriageTarget::Keep,
                };
            }
        }
        for action in &app.actions {
            match action {
                rules::Action::Delete => {
                    target = TriageTarget::Trash;
                    break;
                }
                rules::Action::Move { target: folder } => {
                    let lowered = folder.to_ascii_lowercase();
                    if lowered.contains("trash") || lowered.contains("deleted") {
                        target = TriageTarget::Trash;
                    } else if lowered.contains("archive") {
                        target = TriageTarget::Archive;
                    }
                }
                _ => {}
            }
        }
        if !matches!(target, TriageTarget::Keep) {
            let confidence = by_name
                .get(app.rule_name.as_str())
                .and_then(|r| r.triage_confidence)
                .unwrap_or(0.95);
            out.insert(
                app.email_id,
                (
                    target,
                    format!("matched rule '{}'", app.rule_name),
                    confidence,
                ),
            );
        }
    }
    out
}

fn triage_from_heuristics(email: &Email) -> (TriageTarget, String, f32) {
    let from = email
        .from
        .as_ref()
        .and_then(|f| f.first())
        .and_then(|f| f.email.as_deref())
        .unwrap_or("")
        .to_ascii_lowercase();
    let subject = email.subject.as_deref().unwrap_or("").to_ascii_lowercase();

    let trash_domains = [
        "equityexcelloans.com",
        "megastarfinancial.com",
        "fastrefinow.com",
        "homebridgerefi.com",
        "homefinancerelief.com",
        "communitymortgagehelp.com",
        "bridgepointloans.com",
        "cecilia.in",
    ];
    if trash_domains.iter().any(|d| from.contains(d)) {
        return (
            TriageTarget::Trash,
            "known spam-like sender".to_string(),
            0.92,
        );
    }
    if subject.contains("lower your monthly payments")
        || subject.contains("payment reduction")
        || subject.contains("harp program")
    {
        return (
            TriageTarget::Trash,
            "mortgage/refi spam pattern".to_string(),
            0.9,
        );
    }

    let archive_domains = [
        "delta.com",
        "alaskaair.com",
        "americanexpress.com",
        "chase.com",
        "bankofamerica.com",
        "orders.icebreaker.com",
        "ebay.com",
        "collinstreet.com",
        "hyatt.com",
        "anthropic.com",
        "accounts.google.com",
        "ui.com",
        "github.com",
        "gitlab@redox-os.org",
        "comcast.net",
    ];
    if archive_domains.iter().any(|d| from.contains(d)) {
        return (
            TriageTarget::Archive,
            "transactional/notification sender".to_string(),
            0.82,
        );
    }
    if subject.contains("receipt")
        || subject.contains("statement")
        || subject.contains("security alert")
        || subject.contains("shipment")
        || subject.contains("order confirmed")
        || subject.contains("dependabot")
    {
        return (
            TriageTarget::Archive,
            "transactional/notification subject".to_string(),
            0.78,
        );
    }

    (
        TriageTarget::Keep,
        "no archive/trash signal".to_string(),
        0.4,
    )
}

fn cmd_triage_suggest(state: &mut CliState, input: &Value) -> Value {
    let mailbox_id = match input.get("mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'mailbox_id' field"),
    };
    let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
    let position = input.get("position").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let search = input
        .get("search")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let received_after = input
        .get("received_after")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let received_before = input
        .get("received_before")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Err(e) = state.send_cmd(BackendCommand::QueryEmails {
        origin: "cli:triage".to_string(),
        mailbox_id: mailbox_id.clone(),
        page_size: limit,
        position,
        search_query: search,
        received_after,
        received_before,
    }) {
        return err_response(&e);
    }

    let emails = match state.recv_resp() {
        Ok(BackendResponse::Emails {
            emails: Ok(list), ..
        }) => list,
        Ok(BackendResponse::Emails { emails: Err(e), .. }) => return err_response(&e),
        Ok(_) => return err_response("unexpected response from backend"),
        Err(e) => return err_response(&e),
    };

    let rule_targets = triage_from_rules(state, &emails);
    let mut archive = Vec::new();
    let mut trash = Vec::new();
    let mut keep = Vec::new();

    for email in emails {
        let (target, reason, confidence) = if let Some((t, r, c)) = rule_targets.get(&email.id) {
            (*t, r.clone(), *c)
        } else {
            triage_from_heuristics(&email)
        };
        let from_email = email
            .from
            .as_ref()
            .and_then(|f| f.first())
            .and_then(|a| a.email.clone());
        let item = TriageItem {
            id: email.id.clone(),
            received_at: email.received_at.clone(),
            from: from_email,
            subject: email.subject.clone(),
            target,
            reason,
            confidence,
        };
        match item.target {
            TriageTarget::Archive => archive.push(item),
            TriageTarget::Trash => trash.push(item),
            TriageTarget::Keep => keep.push(item),
        }
    }

    let plan_id = state.next_plan_id();
    let plan = TriagePlan {
        archive_ids: archive.iter().map(|i| i.id.clone()).collect(),
        trash_ids: trash.iter().map(|i| i.id.clone()).collect(),
    };
    state.triage_plans.insert(plan_id.clone(), plan);

    let serialize_items = |items: &[TriageItem]| -> Vec<Value> {
        items
            .iter()
            .map(|i| {
                json!({
                    "id": i.id,
                    "received_at": i.received_at,
                    "from": i.from,
                    "subject": i.subject,
                    "reason": i.reason,
                    "confidence": i.confidence,
                })
            })
            .collect()
    };

    ok_response(json!({
        "plan_id": plan_id,
        "mailbox_id": mailbox_id,
        "archive": serialize_items(&archive),
        "trash": serialize_items(&trash),
        "keep": serialize_items(&keep),
    }))
}

fn cmd_apply_triage_plan(state: &mut CliState, input: &Value) -> Value {
    let (archive_ids, trash_ids) =
        if let Some(plan_id) = input.get("plan_id").and_then(|v| v.as_str()) {
            let Some(plan) = state.triage_plans.get(plan_id) else {
                return err_response(&format!("unknown plan_id '{}'", plan_id));
            };
            (plan.archive_ids.clone(), plan.trash_ids.clone())
        } else {
            let archive_ids = match input.get("archive_ids").and_then(|v| v.as_array()) {
                Some(v) => v
                    .iter()
                    .filter_map(|id| id.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };
            let trash_ids = match input.get("trash_ids").and_then(|v| v.as_array()) {
                Some(v) => v
                    .iter()
                    .filter_map(|id| id.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };
            if archive_ids.is_empty() && trash_ids.is_empty() {
                return err_response("provide 'plan_id' or non-empty archive_ids/trash_ids");
            }
            (archive_ids, trash_ids)
        };

    let archive_folder = state.archive_folder.clone();
    let archive_mailbox_id = state.archive_mailbox_id.clone();
    let archive_target =
        match state.resolve_folder_id(&archive_folder, archive_mailbox_id.as_deref()) {
            Some(id) => id,
            None => {
                return err_response(&format!(
                    "cannot resolve archive folder '{}'",
                    archive_folder
                ))
            }
        };

    let deleted_folder = state.deleted_folder.clone();
    let deleted_mailbox_id = state.deleted_mailbox_id.clone();
    let trash_target = match state.resolve_folder_id(&deleted_folder, deleted_mailbox_id.as_deref())
    {
        Some(id) => id,
        None => {
            return err_response(&format!(
                "cannot resolve deleted folder '{}'",
                deleted_folder
            ))
        }
    };

    let archive_resp = mutate_many_move(state, &archive_ids, &archive_target);
    let trash_resp = mutate_many_move(state, &trash_ids, &trash_target);

    ok_response(json!({
        "archive": archive_resp,
        "trash": trash_resp,
    }))
}

fn cmd_destroy(state: &mut CliState, input: &Value) -> Value {
    let ids = match input.get("ids").and_then(|v| v.as_array()) {
        Some(arr) => {
            let mut ids = Vec::new();
            for v in arr {
                match v.as_str() {
                    Some(s) => ids.push(s.to_string()),
                    None => return err_response("'ids' must be an array of strings"),
                }
            }
            ids
        }
        None => {
            // Support single id too
            match input.get("id").and_then(|v| v.as_str()) {
                Some(id) => vec![id.to_string()],
                None => return err_response("missing 'ids' or 'id' field"),
            }
        }
    };

    let mut results = Vec::new();
    for id in &ids {
        let op_id = state.next_op_id();
        if let Err(e) = state.send_cmd(BackendCommand::DestroyEmail {
            op_id,
            id: id.clone(),
        }) {
            results.push(json!({"id": id, "ok": false, "error": e}));
            continue;
        }
        match state.recv_resp() {
            Ok(BackendResponse::EmailMutation { result, id, .. }) => match result {
                Ok(()) => results.push(json!({"id": id, "ok": true})),
                Err(e) => results.push(json!({"id": id, "ok": false, "error": e})),
            },
            Ok(_) => results.push(json!({"id": id, "ok": false, "error": "unexpected response"})),
            Err(e) => results.push(json!({"id": id, "ok": false, "error": e})),
        }
    }

    ok_response(json!({"results": results}))
}

fn cmd_mark_mailbox_read(state: &mut CliState, input: &Value) -> Value {
    let mailbox_id = match input.get("mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'mailbox_id' field"),
    };

    let mailbox_name = state
        .cached_mailboxes
        .iter()
        .find(|m| m.id == mailbox_id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| mailbox_id.clone());

    if let Err(e) = state.send_cmd(BackendCommand::MarkMailboxRead {
        mailbox_id,
        mailbox_name,
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::MailboxMarkedRead {
            mailbox_name,
            updated,
            result,
            ..
        }) => match result {
            Ok(()) => ok_response(json!({
                "mailbox_name": mailbox_name,
                "updated": updated,
            })),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_get_raw_headers(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };

    if let Err(e) = state.send_cmd(BackendCommand::GetEmailRawHeaders { id }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::EmailRawHeaders { result, .. }) => match result {
            Ok(headers) => ok_response(json!({"headers": headers})),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_download_attachment(state: &mut CliState, input: &Value) -> Value {
    let blob_id = match input.get("blob_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'blob_id' field"),
    };
    let name = input
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("attachment")
        .to_string();
    let content_type = input
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream")
        .to_string();

    if let Err(e) = state.send_cmd(BackendCommand::DownloadAttachment {
        blob_id,
        name,
        content_type,
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::AttachmentDownloaded { name, result }) => match result {
            Ok(path) => ok_response(json!({
                "name": name,
                "path": path.to_string_lossy(),
            })),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_compose_draft(state: &CliState) -> Value {
    let from = state.reply_from_header();
    let draft = compose::build_compose_draft(&from);
    ok_response(json!({"draft": draft}))
}

fn cmd_reply_draft(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let reply_all = input
        .get("reply_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Err(e) = state.send_cmd(BackendCommand::GetEmailForReply { id }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::EmailForReply {
            result: boxed_result,
            ..
        }) => match *boxed_result {
            Ok(email) => {
                let from = state.reply_from_header();
                let draft = compose::build_reply_draft(&email, reply_all, &from);
                ok_response(json!({"draft": draft}))
            }
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_forward_draft(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };

    if let Err(e) = state.send_cmd(BackendCommand::GetEmailForReply { id }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::EmailForReply {
            result: boxed_result,
            ..
        }) => match *boxed_result {
            Ok(email) => {
                let from = state.reply_from_header();
                let draft = compose::build_forward_draft(&email, &from);
                ok_response(json!({"draft": draft}))
            }
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn cmd_keybindings() -> Value {
    let bindings = keybindings::all_keybindings();
    let list: Vec<Value> = bindings
        .iter()
        .map(|kb| {
            json!({
                "view": kb.view,
                "key": kb.key,
                "action": kb.action,
                "description": kb.description,
            })
        })
        .collect();
    ok_response(json!({"keybindings": list}))
}

/// Train the spam classifier on a message. `spam` may be a boolean
/// (`true`=spam, `false`=ham) or the string `"spam"`/`"ham"`.
fn cmd_train(state: &mut CliState, input: &Value) -> Value {
    let id = match input.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'id' field"),
    };
    let spam = match input.get("spam") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Str(s)) if s == "spam" => true,
        Some(Value::Str(s)) if s == "ham" => false,
        _ => return err_response("missing 'spam' field (true/false, or \"spam\"/\"ham\")"),
    };

    if let Err(e) = state.send_cmd(BackendCommand::TrainMessage {
        origin: "cli".to_string(),
        id: id.clone(),
        spam,
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::MessageTrained { spam, result }) => match result {
            Ok(()) => ok_response(json!({
                "id": id,
                "trained_as": if spam { "spam" } else { "ham" },
            })),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

/// Parse the `spam` field shared by train commands: bool, or "spam"/"ham".
fn parse_spam_flag(input: &Value) -> Result<bool, String> {
    match input.get("spam") {
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::Str(s)) if s == "spam" => Ok(true),
        Some(Value::Str(s)) if s == "ham" => Ok(false),
        _ => Err("missing 'spam' field (true/false, or \"spam\"/\"ham\")".to_string()),
    }
}

/// Bulk-train every message in a mailbox with one label.
fn cmd_train_mailbox(state: &mut CliState, input: &Value) -> Value {
    let mailbox_id = match input.get("mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'mailbox_id' field"),
    };
    let spam = match parse_spam_flag(input) {
        Ok(s) => s,
        Err(e) => return err_response(&e),
    };
    let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    if let Err(e) = state.send_cmd(BackendCommand::TrainMailbox {
        origin: "cli".to_string(),
        mailbox_id: mailbox_id.clone(),
        spam,
        limit,
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::MailboxTrained { spam, result }) => match result {
            Ok((trained, failed)) => ok_response(json!({
                "mailbox_id": mailbox_id,
                "trained_as": if spam { "spam" } else { "ham" },
                "trained": trained,
                "failed": failed,
            })),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

/// Read-only classification of a single message (`id`) or a mailbox sample
/// (`mailbox_id` + optional `limit`). Takes no action.
fn cmd_classify(state: &mut CliState, input: &Value) -> Value {
    if let Some(id) = input.get("id").and_then(|v| v.as_str()) {
        let id = id.to_string();
        if let Err(e) = state.send_cmd(BackendCommand::ClassifyMessage {
            origin: "cli".to_string(),
            id: id.clone(),
        }) {
            return err_response(&e);
        }
        return match state.recv_resp() {
            Ok(BackendResponse::MessageClassified { id, result }) => match result {
                Ok((score, verdict)) => ok_response(json!({
                    "id": id,
                    "score": score,
                    "verdict": verdict,
                })),
                Err(e) => err_response(&e),
            },
            Ok(_) => err_response("unexpected response from backend"),
            Err(e) => err_response(&e),
        };
    }

    let mailbox_id = match input.get("mailbox_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return err_response("missing 'mailbox_id' or 'id' field"),
    };
    let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as u32;

    if let Err(e) = state.send_cmd(BackendCommand::ClassifyMailbox {
        origin: "cli".to_string(),
        mailbox_id: mailbox_id.clone(),
        limit,
    }) {
        return err_response(&e);
    }

    match state.recv_resp() {
        Ok(BackendResponse::MailboxClassified { result }) => match result {
            Ok(entries) => {
                let results: Vec<Value> = entries
                    .iter()
                    .map(|e| {
                        json!({
                            "id": e.id,
                            "from": e.from,
                            "subject": e.subject,
                            "score": e.score,
                            "verdict": e.verdict,
                        })
                    })
                    .collect();
                ok_response(json!({
                    "mailbox_id": mailbox_id,
                    "count": results.len(),
                    "results": results,
                }))
            }
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

fn recv_mutation_response(state: &CliState) -> Value {
    match state.recv_resp() {
        Ok(BackendResponse::EmailMutation {
            op_id,
            id,
            action,
            result,
        }) => match result {
            Ok(()) => ok_response(json!({
                "op_id": op_id,
                "id": id,
                "action": format!("{:?}", action),
            })),
            Err(e) => err_response(&e),
        },
        Ok(_) => err_response("unexpected response from backend"),
        Err(e) => err_response(&e),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_cli(
    config: Config,
    rules: Vec<CompiledRule>,
    custom_headers: Vec<String>,
    rules_mailbox_regex: UserRegex,
    my_email_regex: UserRegex,
    archive_folder: String,
    deleted_folder: String,
    archive_mailbox_id: Option<String>,
    deleted_mailbox_id: Option<String>,
    offline: bool,
) {
    let mut state = CliState {
        config,
        cmd_tx: None,
        resp_rx: None,
        connected_account: None,
        connected_username: None,
        cached_mailboxes: Vec::new(),
        next_op_id: 0,
        rules: Arc::new(rules),
        custom_headers: Arc::new(custom_headers),
        rules_mailbox_regex: Arc::new(rules_mailbox_regex),
        my_email_regex: Arc::new(my_email_regex),
        archive_folder,
        deleted_folder,
        archive_mailbox_id,
        deleted_mailbox_id,
        next_plan_id: 0,
        triage_plans: HashMap::new(),
        offline,
    };

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let input = match json::parse(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = err_response(&format!("JSON parse error: {}", e));
                let _ = stdout.write_all(&resp.to_vec());
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
                continue;
            }
        };

        let response = dispatch(&mut state, &input);
        let _ = stdout.write_all(&response.to_vec());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }

    // Shutdown backend on exit
    if let Some(ref tx) = state.cmd_tx {
        let _ = tx.send(BackendCommand::Shutdown);
    }
}

pub fn print_help_cli() {
    print!(
        r#"td-mail --cli: JSON-over-stdin/stdout CLI mode
==========================================

Protocol: Newline-Delimited JSON (NDJSON)
- Send one JSON object per line to stdin
- Receive one JSON response per line from stdout
- Responses have {{"ok": true, ...}} on success or {{"ok": false, "error": "..."}} on failure

Connection Flow
---------------
1. List available accounts:
   > {{"command": "list_accounts"}}
   < {{"ok": true, "accounts": [{{"name": "personal", "username": "me@example.com", ...}}]}}

2. Connect to an account:
   > {{"command": "connect", "account": "personal"}}
   < {{"ok": true, "account": "personal", "username": "me@example.com"}}

3. Check status:
   > {{"command": "status"}}
   < {{"ok": true, "connected": true, "account": "personal", "username": "me@example.com", "cached_mailboxes": 0}}

Mailbox Commands
----------------
list_mailboxes: Fetch and cache all mailboxes.
   > {{"command": "list_mailboxes"}}
   < {{"ok": true, "mailboxes": [{{"id": "...", "name": "INBOX", "role": "inbox", "total_emails": 42, "unread_emails": 3, ...}}]}}

create_mailbox: Create a new mailbox.
   > {{"command": "create_mailbox", "name": "NewFolder"}}
   < {{"ok": true, "name": "NewFolder"}}

delete_mailbox: Delete a mailbox by ID.
   > {{"command": "delete_mailbox", "mailbox_id": "mbox-id"}}
   < {{"ok": true, "name": "NewFolder"}}

mark_mailbox_read: Mark all emails in a mailbox as read.
   > {{"command": "mark_mailbox_read", "mailbox_id": "mbox-id"}}
   < {{"ok": true, "mailbox_name": "INBOX", "updated": 15}}

Email Query Commands
--------------------
query_emails: Query emails in a mailbox.
   > {{"command": "query_emails", "mailbox_id": "mbox-id", "limit": 50, "position": 0, "search": null}}
   Optional: headers_only (bool), max_body_chars (int), received_after (RFC3339/date), received_before (RFC3339/date)
   < {{"ok": true, "emails": [...], "total": 100, "position": 0, "loaded": 50, "thread_counts": {{...}}}}

get_email: Fetch a single email with full body.
   > {{"command": "get_email", "id": "email-id"}}
   Optional: headers_only (bool, default false), max_body_chars (int, 0=unlimited)
   < {{"ok": true, "id": "...", "subject": "...", "body": "...", "body_truncated": false, ...}}

get_thread: Fetch all emails in a thread.
   > {{"command": "get_thread", "thread_id": "thread-id"}}
   Optional: headers_only (bool), max_body_chars (int)
   < {{"ok": true, "thread_id": "...", "emails": [...]}}

get_raw_headers: Get raw RFC headers for an email.
   > {{"command": "get_raw_headers", "id": "email-id"}}
   < {{"ok": true, "headers": "From: ...\nTo: ...\n..."}}

Context Control
---------------
Both get_email, get_thread, and query_emails accept:
  - "headers_only": true — omit body/preview, return only metadata
  - "max_body_chars": 500 — truncate body text; response includes "body_truncated": true if truncated

Email Mutation Commands
-----------------------
mark_read:    {{"command": "mark_read", "id": "email-id"}}
mark_unread:  {{"command": "mark_unread", "id": "email-id"}}
flag:         {{"command": "flag", "id": "email-id"}}
unflag:       {{"command": "unflag", "id": "email-id"}}
move_email:   {{"command": "move_email", "id": "email-id", "to_mailbox_id": "mbox-id"}}
bulk_move:    {{"command": "bulk_move", "ids": ["id1", "id2"], "to_mailbox_id": "mbox-id"}}
archive:      {{"command": "archive", "id": "email-id"}}  (uses configured archive folder)
bulk_archive: {{"command": "bulk_archive", "ids": ["id1", "id2"]}}
delete_email: {{"command": "delete_email", "id": "email-id"}}  (uses configured deleted folder)
bulk_delete_email: {{"command": "bulk_delete_email", "ids": ["id1", "id2"]}}
destroy:      {{"command": "destroy", "ids": ["id1", "id2"]}}  (permanently delete)

Triage Automation
-----------------
triage_suggest: Dry-run proposal for archive/trash/keep with reasons.
   > {{"command": "triage_suggest", "mailbox_id": "mbox-id", "limit": 200, "received_after": "2025-12-01", "received_before": "2026-01-01"}}
   < {{"ok": true, "plan_id": "plan-1", "archive": [...], "trash": [...], "keep": [...]}}

apply_triage_plan: Apply a saved plan safely, or explicit approved IDs.
   > {{"command": "apply_triage_plan", "plan_id": "plan-1"}}
   > {{"command": "apply_triage_plan", "archive_ids": ["id1"], "trash_ids": ["id2"]}}

train: Train the built-in spam classifier on a message (spam: true/false or "spam"/"ham").
   > {{"command": "train", "id": "email-id", "spam": true}}
   < {{"ok": true, "id": "email-id", "trained_as": "spam"}}

train_mailbox: Bulk-train every message in a mailbox with one label (limit 0 = all).
   > {{"command": "train_mailbox", "mailbox_id": "mbox-id", "spam": "ham", "limit": 50}}
   < {{"ok": true, "trained_as": "ham", "trained": 50, "failed": 0}}

classify: Read-only score of one message ("id") or a mailbox sample (takes no action).
   > {{"command": "classify", "id": "email-id"}}
   < {{"ok": true, "id": "email-id", "score": 0.97, "verdict": "spam"}}
   > {{"command": "classify", "mailbox_id": "mbox-id", "limit": 20}}
   < {{"ok": true, "count": 20, "results": [{{"id": "..", "score": 0.97, "verdict": "spam"}}]}}

All mutations return: {{"ok": true, "op_id": N, "id": "...", "action": "..."}}

Attachment Commands
-------------------
download_attachment: Download an attachment blob.
   > {{"command": "download_attachment", "blob_id": "blob-id", "name": "file.pdf", "content_type": "application/pdf"}}
   < {{"ok": true, "name": "file.pdf", "path": "/tmp/td-mail-attachments/file.pdf"}}

Compose Commands
----------------
compose_draft: Generate a blank compose template.
   > {{"command": "compose_draft"}}
   < {{"ok": true, "draft": "From: me@example.com\nTo: \nSubject: \n\n"}}

reply_draft: Generate a reply draft.
   > {{"command": "reply_draft", "id": "email-id", "reply_all": false}}
   < {{"ok": true, "draft": "From: ...\nTo: ...\nSubject: Re: ...\n\n> ..."}}

forward_draft: Generate a forward draft.
   > {{"command": "forward_draft", "id": "email-id"}}
   < {{"ok": true, "draft": "From: ...\nTo: \nSubject: Fwd: ...\n\n---------- Forwarded message ----------\n..."}}

Keybindings
-----------
keybindings: Export the TUI keybinding dictionary.
   > {{"command": "keybindings"}}
   < {{"ok": true, "keybindings": [{{"view": "global", "key": "?", "action": "help", "description": "Show help"}}, ...]}}
"#
    );
}
