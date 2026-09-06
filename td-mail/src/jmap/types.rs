use crate::json::{Json, ObjectBuilder, ToJson};
use std::collections::HashMap;

/// A JSON document did not have the shape a type needs. The text follows
/// serde's, so a diagnostic a user has seen before still reads the same.
pub type JsonError = String;

fn missing(field: &str) -> JsonError {
    format!("missing field `{}`", field)
}

fn wrong_type(field: &str, expected: &str) -> JsonError {
    format!("invalid type for `{}`: expected {}", field, expected)
}

/// The object a type reads its fields from.
fn object<'a>(value: &'a Json, what: &str) -> Result<&'a [(String, Json)], JsonError> {
    value.as_obj().ok_or_else(|| wrong_type(what, "an object"))
}

fn field<'a>(obj: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    obj.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// A present, non-null field. serde reads an explicit `null` for an `Option`
/// field as `None` and for a `#[serde(default)]` scalar as the default, so
/// absent and null are treated alike throughout.
fn present<'a>(obj: &'a [(String, Json)], name: &str) -> Option<&'a Json> {
    field(obj, name).filter(|v| !v.is_null())
}

fn req_str(obj: &[(String, Json)], name: &str) -> Result<String, JsonError> {
    match present(obj, name) {
        None => Err(missing(name)),
        Some(v) => v
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| wrong_type(name, "a string")),
    }
}

fn opt_str(obj: &[(String, Json)], name: &str) -> Result<Option<String>, JsonError> {
    match present(obj, name) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| wrong_type(name, "a string")),
    }
}

/// A `#[serde(default)]` string: absent or null reads as empty.
fn def_str(obj: &[(String, Json)], name: &str) -> Result<String, JsonError> {
    Ok(opt_str(obj, name)?.unwrap_or_default())
}

fn def_bool(obj: &[(String, Json)], name: &str) -> Result<bool, JsonError> {
    match present(obj, name) {
        None => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| wrong_type(name, "a boolean")),
    }
}

fn def_u32(obj: &[(String, Json)], name: &str) -> Result<u32, JsonError> {
    match present(obj, name) {
        None => Ok(0),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| wrong_type(name, "a u32")),
    }
}

fn opt_u32(obj: &[(String, Json)], name: &str) -> Result<Option<u32>, JsonError> {
    match present(obj, name) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| wrong_type(name, "a u32")),
    }
}

fn opt_u64(obj: &[(String, Json)], name: &str) -> Result<Option<u64>, JsonError> {
    match present(obj, name) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| wrong_type(name, "a u64")),
    }
}

fn req_arr<'a>(obj: &'a [(String, Json)], name: &str) -> Result<&'a [Json], JsonError> {
    match present(obj, name) {
        None => Err(missing(name)),
        Some(v) => v.as_arr().ok_or_else(|| wrong_type(name, "an array")),
    }
}

fn strs(value: &Json, name: &str) -> Result<Vec<String>, JsonError> {
    let items = value.as_arr().ok_or_else(|| wrong_type(name, "an array"))?;
    items
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| wrong_type(name, "an array of strings"))
        })
        .collect()
}

/// A `#[serde(default)]` `Vec<String>`.
fn def_strs(obj: &[(String, Json)], name: &str) -> Result<Vec<String>, JsonError> {
    match present(obj, name) {
        None => Ok(Vec::new()),
        Some(v) => strs(v, name),
    }
}

fn opt_strs(obj: &[(String, Json)], name: &str) -> Result<Option<Vec<String>>, JsonError> {
    match present(obj, name) {
        None => Ok(None),
        Some(v) => strs(v, name).map(Some),
    }
}

/// A `#[serde(default)]` `HashMap<String, bool>`; JMAP writes `true` for a set
/// keyword or for mailbox membership.
fn def_flags(obj: &[(String, Json)], name: &str) -> Result<HashMap<String, bool>, JsonError> {
    let Some(value) = present(obj, name) else {
        return Ok(HashMap::new());
    };
    let entries = object(value, name)?;
    let mut out = HashMap::with_capacity(entries.len());
    for (key, v) in entries {
        let flag = v.as_bool().ok_or_else(|| wrong_type(name, "a boolean"))?;
        out.insert(key.clone(), flag);
    }
    Ok(out)
}

/// A `#[serde(default)]` `HashMap<String, String>`.
fn def_string_map(
    obj: &[(String, Json)],
    name: &str,
) -> Result<HashMap<String, String>, JsonError> {
    let Some(value) = present(obj, name) else {
        return Ok(HashMap::new());
    };
    let entries = object(value, name)?;
    let mut out = HashMap::with_capacity(entries.len());
    for (key, v) in entries {
        let text = v.as_str().ok_or_else(|| wrong_type(name, "a string"))?;
        out.insert(key.clone(), text.to_string());
    }
    Ok(out)
}

fn read_list<T>(
    items: &[Json],
    read: fn(&Json) -> Result<T, JsonError>,
) -> Result<Vec<T>, JsonError> {
    items.iter().map(read).collect()
}

fn opt_list<T>(
    obj: &[(String, Json)],
    name: &str,
    read: fn(&Json) -> Result<T, JsonError>,
) -> Result<Option<Vec<T>>, JsonError> {
    match present(obj, name) {
        None => Ok(None),
        Some(v) => {
            let items = v.as_arr().ok_or_else(|| wrong_type(name, "an array"))?;
            read_list(items, read).map(Some)
        }
    }
}

// JMAP Session (from .well-known/jmap)
#[derive(Debug)]
pub struct JmapSession {
    pub username: String,
    pub api_url: String,
    pub download_url: Option<String>,
    pub primary_accounts: HashMap<String, String>,
    pub accounts: HashMap<String, JmapAccount>,
}

impl JmapSession {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "JmapSession")?;
        let accounts = match present(obj, "accounts") {
            None => HashMap::new(),
            Some(v) => {
                let entries = object(v, "accounts")?;
                let mut out = HashMap::with_capacity(entries.len());
                for (key, account) in entries {
                    out.insert(key.clone(), JmapAccount::from_json(account)?);
                }
                out
            }
        };
        Ok(JmapSession {
            username: req_str(obj, "username")?,
            api_url: req_str(obj, "apiUrl")?,
            download_url: opt_str(obj, "downloadUrl")?,
            primary_accounts: def_string_map(obj, "primaryAccounts")?,
            accounts,
        })
    }
}

#[derive(Debug)]
pub struct JmapAccount {
    pub name: String,
    pub is_personal: bool,
    pub is_read_only: bool,
}

impl JmapAccount {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "JmapAccount")?;
        Ok(JmapAccount {
            name: req_str(obj, "name")?,
            is_personal: def_bool(obj, "isPersonal")?,
            is_read_only: def_bool(obj, "isReadOnly")?,
        })
    }
}

impl JmapSession {
    pub fn mail_account_id(&self) -> Option<&str> {
        // First try the standard primaryAccounts lookup
        if let Some(id) = self.primary_accounts.get("urn:ietf:params:jmap:mail") {
            return Some(id.as_str());
        }

        // Fallback: if there's exactly one account, use that
        if self.accounts.len() == 1 {
            return self.accounts.keys().next().map(|s| s.as_str());
        }

        None
    }
}

// JMAP Request/Response
#[derive(Debug)]
pub struct JmapRequest {
    pub using: Vec<&'static str>,
    pub method_calls: Vec<MethodCall>,
}

impl ToJson for JmapRequest {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("using", &self.using)
            .set("methodCalls", &self.method_calls)
            .build()
    }
}

#[derive(Debug)]
pub struct MethodCall(pub &'static str, pub Json, pub String);

impl ToJson for MethodCall {
    fn to_json(&self) -> Json {
        Json::Arr(vec![
            Json::Str(self.0.to_string()),
            self.1.clone(),
            Json::Str(self.2.clone()),
        ])
    }
}

#[derive(Debug)]
pub struct JmapResponse {
    pub method_responses: Vec<MethodResponse>,
    #[allow(dead_code)]
    pub session_state: String,
}

impl JmapResponse {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "JmapResponse")?;
        let calls = req_arr(obj, "methodResponses")?;
        Ok(JmapResponse {
            method_responses: read_list(calls, MethodResponse::from_json)?,
            session_state: def_str(obj, "sessionState")?,
        })
    }
}

#[derive(Debug)]
pub struct MethodResponse(pub String, pub Json, #[allow(dead_code)] pub String);

impl MethodResponse {
    fn from_json(value: &Json) -> Result<Self, JsonError> {
        let items = value
            .as_arr()
            .ok_or_else(|| wrong_type("MethodResponse", "an array"))?;
        let name = items
            .first()
            .and_then(Json::as_str)
            .ok_or_else(|| wrong_type("MethodResponse", "a name"))?;
        let arguments = items
            .get(1)
            .ok_or_else(|| wrong_type("MethodResponse", "arguments"))?;
        let call_id = items
            .get(2)
            .and_then(Json::as_str)
            .ok_or_else(|| wrong_type("MethodResponse", "a call id"))?;
        Ok(MethodResponse(
            name.to_string(),
            arguments.clone(),
            call_id.to_string(),
        ))
    }
}

// Mailbox types
#[derive(Debug, Clone)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub role: Option<String>,
    pub total_emails: u32,
    pub unread_emails: u32,
    pub sort_order: u32,
}

impl Mailbox {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "Mailbox")?;
        Ok(Mailbox {
            id: req_str(obj, "id")?,
            name: req_str(obj, "name")?,
            parent_id: opt_str(obj, "parentId")?,
            role: opt_str(obj, "role")?,
            total_emails: def_u32(obj, "totalEmails")?,
            unread_emails: def_u32(obj, "unreadEmails")?,
            sort_order: def_u32(obj, "sortOrder")?,
        })
    }
}

impl ToJson for Mailbox {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("id", &self.id)
            .set("name", &self.name)
            .set("parentId", &self.parent_id)
            .set("role", &self.role)
            .set("totalEmails", self.total_emails)
            .set("unreadEmails", self.unread_emails)
            .set("sortOrder", self.sort_order)
            .build()
    }
}

#[derive(Debug)]
pub struct MailboxGetResponse {
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub state: String,
    pub list: Vec<Mailbox>,
    #[allow(dead_code)]
    pub not_found: Vec<String>,
}

impl MailboxGetResponse {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "MailboxGetResponse")?;
        Ok(MailboxGetResponse {
            account_id: req_str(obj, "accountId")?,
            state: req_str(obj, "state")?,
            list: read_list(req_arr(obj, "list")?, Mailbox::from_json)?,
            not_found: def_strs(obj, "notFound")?,
        })
    }
}

// Email types
#[derive(Debug)]
pub struct EmailQueryResponse {
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub query_state: String,
    pub ids: Vec<String>,
    pub position: u32,
    pub total: Option<u32>,
}

impl EmailQueryResponse {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "EmailQueryResponse")?;
        let ids = present(obj, "ids").ok_or_else(|| missing("ids"))?;
        Ok(EmailQueryResponse {
            account_id: req_str(obj, "accountId")?,
            query_state: req_str(obj, "queryState")?,
            ids: strs(ids, "ids")?,
            position: def_u32(obj, "position")?,
            total: opt_u32(obj, "total")?,
        })
    }
}

#[derive(Debug)]
pub struct EmailQueryResult {
    pub ids: Vec<String>,
    pub total: Option<u32>,
    pub position: u32,
}

/// The JMAP properties `Email` reads into named fields. Anything else the
/// server sends is kept in `extra` and written back out unchanged, which is
/// what `#[serde(flatten)]` did.
const EMAIL_KNOWN: &[&str] = &[
    "id",
    "threadId",
    "from",
    "to",
    "cc",
    "replyTo",
    "subject",
    "receivedAt",
    "sentAt",
    "preview",
    "textBody",
    "htmlBody",
    "bodyValues",
    "keywords",
    "mailboxIds",
    "messageId",
    "references",
    "attachments",
];

#[derive(Debug, Clone)]
pub struct Email {
    pub id: String,
    pub thread_id: Option<String>,
    pub from: Option<Vec<EmailAddress>>,
    pub to: Option<Vec<EmailAddress>>,
    pub cc: Option<Vec<EmailAddress>>,
    pub reply_to: Option<Vec<EmailAddress>>,
    pub subject: Option<String>,
    pub received_at: Option<String>,
    pub sent_at: Option<String>,
    pub preview: Option<String>,
    pub text_body: Option<Vec<BodyPart>>,
    pub html_body: Option<Vec<BodyPart>>,
    pub body_values: HashMap<String, BodyValue>,
    pub keywords: HashMap<String, bool>,
    pub mailbox_ids: HashMap<String, bool>,
    pub message_id: Option<Vec<String>>,
    pub references: Option<Vec<String>>,
    pub attachments: Option<Vec<BodyPart>>,
    /// Every property above is not one of; kept so a cache round-trip and a
    /// custom-header rule both see what the server actually sent.
    pub extra: HashMap<String, Json>,
}

impl Email {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "Email")?;
        let body_values = match present(obj, "bodyValues") {
            None => HashMap::new(),
            Some(v) => {
                let entries = object(v, "bodyValues")?;
                let mut out = HashMap::with_capacity(entries.len());
                for (key, body) in entries {
                    out.insert(key.clone(), BodyValue::from_json(body)?);
                }
                out
            }
        };
        let mut extra = HashMap::new();
        for (key, v) in obj {
            if !EMAIL_KNOWN.contains(&key.as_str()) {
                extra.insert(key.clone(), v.clone());
            }
        }
        Ok(Email {
            id: req_str(obj, "id")?,
            thread_id: opt_str(obj, "threadId")?,
            from: opt_list(obj, "from", EmailAddress::from_json)?,
            to: opt_list(obj, "to", EmailAddress::from_json)?,
            cc: opt_list(obj, "cc", EmailAddress::from_json)?,
            reply_to: opt_list(obj, "replyTo", EmailAddress::from_json)?,
            subject: opt_str(obj, "subject")?,
            received_at: opt_str(obj, "receivedAt")?,
            sent_at: opt_str(obj, "sentAt")?,
            preview: opt_str(obj, "preview")?,
            text_body: opt_list(obj, "textBody", BodyPart::from_json)?,
            html_body: opt_list(obj, "htmlBody", BodyPart::from_json)?,
            body_values,
            keywords: def_flags(obj, "keywords")?,
            mailbox_ids: def_flags(obj, "mailboxIds")?,
            message_id: opt_strs(obj, "messageId")?,
            references: opt_strs(obj, "references")?,
            attachments: opt_list(obj, "attachments", BodyPart::from_json)?,
            extra,
        })
    }
}

impl ToJson for Email {
    fn to_json(&self) -> Json {
        let mut obj = ObjectBuilder::new()
            .set("id", &self.id)
            .set("threadId", &self.thread_id)
            .set("from", &self.from)
            .set("to", &self.to)
            .set("cc", &self.cc)
            .set("replyTo", &self.reply_to)
            .set("subject", &self.subject)
            .set("receivedAt", &self.received_at)
            .set("sentAt", &self.sent_at)
            .set("preview", &self.preview)
            .set("textBody", &self.text_body)
            .set("htmlBody", &self.html_body)
            .set("bodyValues", &self.body_values)
            .set("keywords", &self.keywords)
            .set("mailboxIds", &self.mailbox_ids)
            .set("messageId", &self.message_id)
            .set("references", &self.references)
            .set("attachments", &self.attachments);
        // Sorted, because a `HashMap`'s own order is not reproducible and a
        // cache entry must re-serialise to the same bytes.
        let mut names: Vec<&String> = self.extra.keys().collect();
        names.sort();
        for name in names {
            if let Some(value) = self.extra.get(name) {
                obj = obj.set(name.clone(), value);
            }
        }
        obj.build()
    }
}

#[derive(Debug, Clone)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl EmailAddress {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "EmailAddress")?;
        Ok(EmailAddress {
            name: opt_str(obj, "name")?,
            email: opt_str(obj, "email")?,
        })
    }
}

impl ToJson for EmailAddress {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("name", &self.name)
            .set("email", &self.email)
            .build()
    }
}

impl std::fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.name, &self.email) {
            (Some(name), Some(email)) => write!(f, "{} <{}>", name, email),
            (None, Some(email)) => write!(f, "{}", email),
            (Some(name), None) => write!(f, "{}", name),
            (None, None) => write!(f, "(unknown)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BodyPart {
    pub part_id: String,
    pub blob_id: Option<String>,
    pub r#type: Option<String>,
    pub name: Option<String>,
    pub size: Option<u64>,
}

impl BodyPart {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "BodyPart")?;
        Ok(BodyPart {
            part_id: req_str(obj, "partId")?,
            blob_id: opt_str(obj, "blobId")?,
            r#type: opt_str(obj, "type")?,
            name: opt_str(obj, "name")?,
            size: opt_u64(obj, "size")?,
        })
    }
}

impl ToJson for BodyPart {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("partId", &self.part_id)
            .set("blobId", &self.blob_id)
            .set("type", &self.r#type)
            .set("name", &self.name)
            .set("size", self.size)
            .build()
    }
}

#[derive(Debug, Clone)]
pub struct BodyValue {
    pub value: String,
    pub is_encoding_problem: bool,
    pub is_truncated: bool,
}

impl BodyValue {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "BodyValue")?;
        Ok(BodyValue {
            value: req_str(obj, "value")?,
            is_encoding_problem: def_bool(obj, "isEncodingProblem")?,
            is_truncated: def_bool(obj, "isTruncated")?,
        })
    }
}

impl ToJson for BodyValue {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("value", &self.value)
            .set("isEncodingProblem", self.is_encoding_problem)
            .set("isTruncated", self.is_truncated)
            .build()
    }
}

#[derive(Debug)]
pub struct EmailGetResponse {
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub state: String,
    pub list: Vec<Email>,
    #[allow(dead_code)]
    pub not_found: Vec<String>,
}

impl EmailGetResponse {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "EmailGetResponse")?;
        Ok(EmailGetResponse {
            account_id: req_str(obj, "accountId")?,
            state: req_str(obj, "state")?,
            list: read_list(req_arr(obj, "list")?, Email::from_json)?,
            not_found: def_strs(obj, "notFound")?,
        })
    }
}

// Thread types
#[derive(Debug)]
pub struct Thread {
    pub id: String,
    pub email_ids: Vec<String>,
}

impl Thread {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "Thread")?;
        let ids = present(obj, "emailIds").ok_or_else(|| missing("emailIds"))?;
        Ok(Thread {
            id: req_str(obj, "id")?,
            email_ids: strs(ids, "emailIds")?,
        })
    }
}

#[derive(Debug)]
pub struct ThreadGetResponse {
    #[allow(dead_code)]
    pub account_id: String,
    #[allow(dead_code)]
    pub state: String,
    pub list: Vec<Thread>,
    #[allow(dead_code)]
    pub not_found: Vec<String>,
}

impl ThreadGetResponse {
    pub fn from_json(value: &Json) -> Result<Self, JsonError> {
        let obj = object(value, "ThreadGetResponse")?;
        Ok(ThreadGetResponse {
            account_id: req_str(obj, "accountId")?,
            state: req_str(obj, "state")?,
            list: read_list(req_arr(obj, "list")?, Thread::from_json)?,
            not_found: def_strs(obj, "notFound")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_minimal_jmap_session() {
        let data = json!({
            "username": "user@example.com",
            "apiUrl": "https://api.example.com/jmap"
        });
        let session = JmapSession::from_json(&data).unwrap();
        assert_eq!(session.username, "user@example.com");
        assert_eq!(session.api_url, "https://api.example.com/jmap");
        assert!(session.download_url.is_none());
        assert!(session.primary_accounts.is_empty());
        assert!(session.accounts.is_empty());
    }

    #[test]
    fn test_deserialize_full_jmap_session() {
        let data = json!({
            "username": "user@example.com",
            "apiUrl": "https://api.example.com/jmap",
            "downloadUrl": "https://api.example.com/download/{blobId}",
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "acc-001"
            },
            "accounts": {
                "acc-001": {
                    "name": "Personal",
                    "isPersonal": true,
                    "isReadOnly": false
                }
            }
        });
        let session = JmapSession::from_json(&data).unwrap();
        assert_eq!(
            session.download_url.as_deref(),
            Some("https://api.example.com/download/{blobId}")
        );
        assert_eq!(session.primary_accounts.len(), 1);
        assert_eq!(session.accounts.len(), 1);
        let acc = &session.accounts["acc-001"];
        assert_eq!(acc.name, "Personal");
        assert!(acc.is_personal);
        assert!(!acc.is_read_only);
    }

    #[test]
    fn test_mail_account_id_primary() {
        let data = json!({
            "username": "u@e.com",
            "apiUrl": "https://api.e.com/jmap",
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "primary-id"
            },
            "accounts": {
                "primary-id": { "name": "Main", "isPersonal": true },
                "other-id": { "name": "Other", "isPersonal": false }
            }
        });
        let session = JmapSession::from_json(&data).unwrap();
        assert_eq!(session.mail_account_id(), Some("primary-id"));
    }

    #[test]
    fn test_mail_account_id_fallback_single() {
        let data = json!({
            "username": "u@e.com",
            "apiUrl": "https://api.e.com/jmap",
            "accounts": {
                "only-one": { "name": "Solo", "isPersonal": true }
            }
        });
        let session = JmapSession::from_json(&data).unwrap();
        assert_eq!(session.mail_account_id(), Some("only-one"));
    }

    #[test]
    fn test_mail_account_id_none_multiple_no_primary() {
        let data = json!({
            "username": "u@e.com",
            "apiUrl": "https://api.e.com/jmap",
            "accounts": {
                "acc-1": { "name": "A", "isPersonal": false },
                "acc-2": { "name": "B", "isPersonal": false }
            }
        });
        let session = JmapSession::from_json(&data).unwrap();
        assert_eq!(session.mail_account_id(), None);
    }

    #[test]
    fn test_deserialize_email_minimal() {
        let data = json!({
            "id": "email-001"
        });
        let email = Email::from_json(&data).unwrap();
        assert_eq!(email.id, "email-001");
        assert!(email.thread_id.is_none());
        assert!(email.from.is_none());
        assert!(email.to.is_none());
        assert!(email.cc.is_none());
        assert!(email.reply_to.is_none());
        assert!(email.subject.is_none());
        assert!(email.received_at.is_none());
        assert!(email.sent_at.is_none());
        assert!(email.preview.is_none());
        assert!(email.text_body.is_none());
        assert!(email.body_values.is_empty());
        assert!(email.keywords.is_empty());
        assert!(email.mailbox_ids.is_empty());
        assert!(email.message_id.is_none());
        assert!(email.references.is_none());
        assert!(email.attachments.is_none());
    }

    #[test]
    fn test_deserialize_email_full() {
        let data = json!({
            "id": "email-002",
            "threadId": "thread-001",
            "from": [{"name": "Alice", "email": "alice@example.com"}],
            "to": [{"name": "Bob", "email": "bob@example.com"}],
            "cc": [{"email": "cc@example.com"}],
            "replyTo": [{"name": "Alice", "email": "alice@example.com"}],
            "subject": "Hello World",
            "receivedAt": "2025-01-01T00:00:00Z",
            "sentAt": "2025-01-01T00:00:00Z",
            "preview": "This is a preview",
            "textBody": [{"partId": "1"}],
            "bodyValues": {"1": {"value": "body text", "isEncodingProblem": false, "isTruncated": false}},
            "keywords": {"$seen": true, "$flagged": true},
            "mailboxIds": {"mbox-1": true},
            "messageId": ["<msg-id@example.com>"],
            "references": ["<ref-1@example.com>"],
            "attachments": [{"partId": "2", "blobId": "blob-1", "type": "application/pdf", "name": "doc.pdf", "size": 1024}]
        });
        let email = Email::from_json(&data).unwrap();
        assert_eq!(email.id, "email-002");
        assert_eq!(email.thread_id.as_deref(), Some("thread-001"));
        assert_eq!(email.subject.as_deref(), Some("Hello World"));
        assert_eq!(email.from.as_ref().unwrap().len(), 1);
        assert_eq!(email.keywords.len(), 2);
        assert_eq!(email.attachments.as_ref().unwrap().len(), 1);
        assert_eq!(
            email.attachments.as_ref().unwrap()[0].name.as_deref(),
            Some("doc.pdf")
        );
    }

    #[test]
    fn unknown_email_properties_survive_a_round_trip() {
        let data = json!({
            "id": "email-003",
            "header:X-Spam-Score:asText": "5.5",
            "size": 4096
        });
        let email = Email::from_json(&data).unwrap();
        assert_eq!(email.extra.len(), 2);
        let written = email.to_json();
        assert_eq!(
            written
                .get("header:X-Spam-Score:asText")
                .and_then(Json::as_str),
            Some("5.5")
        );
        assert_eq!(written.get("size").and_then(Json::as_u64), Some(4096));
        // And re-reading the written form gives the same extras back.
        let again = Email::from_json(&written).unwrap();
        assert_eq!(again.extra.len(), 2);
    }

    #[test]
    fn an_email_re_serialises_to_the_same_bytes() {
        let data = json!({
            "id": "email-004",
            "keywords": {"$seen": true},
            "mailboxIds": {"mbox-1": true},
            "zzz": 1,
            "aaa": 2
        });
        let email = Email::from_json(&data).unwrap();
        let once = email.to_json().to_string();
        let twice = Email::from_json(&email.to_json())
            .unwrap()
            .to_json()
            .to_string();
        assert_eq!(once, twice);
    }

    #[test]
    fn test_deserialize_mailbox_defaults() {
        let data = json!({
            "id": "mbox-1",
            "name": "Inbox"
        });
        let mbox = Mailbox::from_json(&data).unwrap();
        assert_eq!(mbox.id, "mbox-1");
        assert_eq!(mbox.name, "Inbox");
        assert!(mbox.parent_id.is_none());
        assert!(mbox.role.is_none());
        assert_eq!(mbox.total_emails, 0);
        assert_eq!(mbox.unread_emails, 0);
        assert_eq!(mbox.sort_order, 0);
    }

    #[test]
    fn test_deserialize_thread() {
        let data = json!({
            "id": "thread-001",
            "emailIds": ["email-1", "email-2", "email-3"]
        });
        let thread = Thread::from_json(&data).unwrap();
        assert_eq!(thread.id, "thread-001");
        assert_eq!(thread.email_ids, vec!["email-1", "email-2", "email-3"]);
    }

    #[test]
    fn test_deserialize_email_query_response_with_total() {
        let data = json!({
            "accountId": "acc-1",
            "queryState": "state-1",
            "ids": ["e1", "e2"],
            "position": 0,
            "total": 42
        });
        let resp = EmailQueryResponse::from_json(&data).unwrap();
        assert_eq!(resp.ids, vec!["e1", "e2"]);
        assert_eq!(resp.total, Some(42));
        assert_eq!(resp.position, 0);
    }

    #[test]
    fn test_deserialize_email_query_response_without_total() {
        let data = json!({
            "accountId": "acc-1",
            "queryState": "state-1",
            "ids": [],
            "position": 10
        });
        let resp = EmailQueryResponse::from_json(&data).unwrap();
        assert!(resp.ids.is_empty());
        assert_eq!(resp.total, None);
        assert_eq!(resp.position, 10);
    }

    #[test]
    fn a_missing_required_field_reads_as_serde_worded_it() {
        let data = json!({ "name": "Inbox" });
        assert_eq!(
            Mailbox::from_json(&data).unwrap_err(),
            "missing field `id`".to_string()
        );
    }

    #[test]
    fn test_email_address_display_name_and_email() {
        let addr = EmailAddress {
            name: Some("Alice".to_string()),
            email: Some("alice@example.com".to_string()),
        };
        assert_eq!(format!("{}", addr), "Alice <alice@example.com>");
    }

    #[test]
    fn test_email_address_display_email_only() {
        let addr = EmailAddress {
            name: None,
            email: Some("alice@example.com".to_string()),
        };
        assert_eq!(format!("{}", addr), "alice@example.com");
    }

    #[test]
    fn test_email_address_display_name_only() {
        let addr = EmailAddress {
            name: Some("Alice".to_string()),
            email: None,
        };
        assert_eq!(format!("{}", addr), "Alice");
    }

    #[test]
    fn test_email_address_display_unknown() {
        let addr = EmailAddress {
            name: None,
            email: None,
        };
        assert_eq!(format!("{}", addr), "(unknown)");
    }
}
