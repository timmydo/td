use td_json::{json, Json, ObjectBuilder, ToJson};

/// The whole of td-mail's transport is td's fetch service. It holds the TLS
/// trust, the resolver, the timeouts and the body caps; this side holds a
/// unix socket and the framing.
///
/// `None` for a response limit asks for the service's own ceiling rather
/// than a smaller one of td-mail's: a JMAP `Email/get` for a hundred messages
/// with full body values runs past ten megabytes routinely, which is why
/// this client never had a cap of its own.
const RESPONSE_LIMIT: Option<u64> = None;
const FLAG_BATCH_MAX: usize = 256;

// An oversized or incomplete reconciliation remains uncertain; it never
// authorizes another send. Each get stays below td-mta's object limit.
const RECONCILIATION_MAX: usize = 4096;

fn reconciliation_incomplete() -> JmapError {
    JmapError::Api(
        "incomplete or changing submission reconciliation; send outcome remains unknown".into(),
    )
}

fn reconciliation_page(
    body: &Json,
    ids: &mut Vec<String>,
    state: &mut Option<String>,
    total: &mut Option<u64>,
) -> Result<bool, JmapError> {
    let next_state = body
        .get("queryState")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(reconciliation_incomplete)?;
    let next_total = body
        .get("total")
        .and_then(Json::as_u64)
        .filter(|n| *n <= RECONCILIATION_MAX as u64)
        .ok_or_else(reconciliation_incomplete)?;
    let page = body
        .get("ids")
        .and_then(Json::as_array)
        .ok_or_else(reconciliation_incomplete)?;
    if body.get("position").and_then(Json::as_u64) != Some(ids.len() as u64)
        || state.as_deref().is_some_and(|s| s != next_state)
        || total.is_some_and(|n| n != next_total)
        || page.len() > 256
        || ids.len() + page.len() > next_total as usize
        || (page.is_empty() && ids.len() < next_total as usize)
    {
        return Err(reconciliation_incomplete());
    }
    for id in page {
        let id = id
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(reconciliation_incomplete)?;
        if ids.iter().any(|previous| previous == id) {
            return Err(reconciliation_incomplete());
        }
        ids.push(id.into());
    }
    *state = Some(next_state.into());
    *total = Some(next_total);
    Ok(ids.len() == next_total as usize)
}

fn reconciliation_filing_fields(body: &Json) -> Result<(), JmapError> {
    let list = body
        .get("list")
        .and_then(Json::as_array)
        .ok_or_else(reconciliation_incomplete)?;
    for email in list {
        for field in ["keywords", "mailboxIds"] {
            let flags = email
                .get(field)
                .and_then(Json::as_object)
                .ok_or_else(reconciliation_incomplete)?;
            if flags.iter().any(|(_, value)| value.as_bool() != Some(true)) {
                return Err(reconciliation_incomplete());
            }
        }
        let message_id = email
            .get("messageId")
            .ok_or_else(reconciliation_incomplete)?;
        if !message_id.is_null()
            && !message_id
                .as_array()
                .is_some_and(|ids| ids.iter().all(|id| id.as_str().is_some()))
        {
            return Err(reconciliation_incomplete());
        }
    }
    Ok(())
}

fn reconciliation_submissions(
    body: &Json,
    ids: &[String],
) -> Result<Vec<(String, String)>, JmapError> {
    let list = body
        .get("list")
        .and_then(Json::as_array)
        .ok_or_else(reconciliation_incomplete)?;
    let missing = body
        .get("notFound")
        .and_then(Json::as_array)
        .ok_or_else(reconciliation_incomplete)?;
    if !missing.is_empty() || list.len() != ids.len() {
        return Err(reconciliation_incomplete());
    }
    let mut remaining: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let mut submissions = Vec::new();
    for item in list {
        let id = item
            .get("id")
            .and_then(Json::as_str)
            .ok_or_else(reconciliation_incomplete)?;
        let email = item
            .get("emailId")
            .and_then(Json::as_str)
            .filter(|id| valid_id(id))
            .ok_or_else(reconciliation_incomplete)?;
        if !remaining.remove(id) {
            return Err(reconciliation_incomplete());
        }
        submissions.push((id.into(), email.into()));
    }
    Ok(submissions)
}

/// The named diagnostic for a jail or a host with no fetch service, in the
/// style APPLICATIONS.md §X asks for: what is absent, where it was looked
/// for, and what would answer it.
fn no_fetch_service() -> JmapError {
    let socket = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => format!("{}/td-fetch/socket", dir),
        _ => "$XDG_RUNTIME_DIR/td-fetch/socket (XDG_RUNTIME_DIR is unset)".to_string(),
    };
    JmapError::NoFetchService(format!(
        "no td-fetch socket at {}: td-mail fetches through td's fetch service; \
         on a host, ./install-apps installs a td-mail that serves one, or \
         td-net fetchd run --socket PATH",
        socket
    ))
}

// td-mta can rate-limit a burst of authenticated reads. Retry only explicit
// short 429 delays or retiring-worker 503s; never replay transport failures
// or mutation batches.
fn read_backoff(
    mut send: impl FnMut() -> Result<td_fetch_client::Response, td_fetch_client::Error>,
    mut wait: impl FnMut(std::time::Duration),
) -> Result<td_fetch_client::Response, td_fetch_client::Error> {
    let mut retries = 0;
    loop {
        let response = send()?;
        let retry_after = response.header("retry-after");
        let delay = retry_after.and_then(|value| {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            value.parse::<u64>().ok().filter(|n| (1..=5).contains(n))
        });
        let delay =
            delay.or_else(|| (response.status == 503 && retry_after.is_none()).then_some(1));
        if !matches!(response.status, 429 | 503) || retries == 2 {
            return Ok(response);
        }
        let Some(seconds) = delay else {
            return Ok(response);
        };
        retries += 1;
        drop(response);
        wait(std::time::Duration::from_secs(seconds));
    }
}

fn read_batch(request: &JmapRequest) -> bool {
    !request.method_calls.is_empty()
        && request.method_calls.iter().all(|call| {
            matches!(
                call.0,
                "Mailbox/get"
                    | "Mailbox/query"
                    | "Mailbox/changes"
                    | "Email/get"
                    | "Email/query"
                    | "Email/changes"
                    | "Thread/get"
                    | "Thread/changes"
            )
        })
}

/// One GET with the auth header, as a status, the `location` header if any,
/// and the body bytes, through td's fetch service — which a jail carrying
/// `sockets=fetch` has (td's APPLICATIONS.md §W.8). The redirect loops
/// decide what to do with the reply; a status of 400 or more is a reply
/// here, not an error, as the loops expect.
struct Fetched {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

/// GET `url`, with the credential when `auth` carries one. No redirects
/// are followed for us: the service would drop the credential on the
/// way, and the loops below follow with it or without it as the origin
/// rule says.
fn get_with_auth(url: &str, auth: Option<&str>) -> Result<Fetched, JmapError> {
    if !td_fetch_client::available() {
        return Err(no_fetch_service());
    }
    let headers: Vec<(&str, &str)> = auth.map(|a| ("authorization", a)).into_iter().collect();
    let response = read_backoff(
        || td_fetch_client::get(url, &headers, RESPONSE_LIMIT, Some(0)),
        std::thread::sleep,
    )
    .map_err(|e| JmapError::Http(e.to_string()))?;
    Ok(Fetched {
        status: response.status,
        location: response.header("location").map(str::to_string),
        body: response.body,
    })
}

use super::types::*;

/// Percent-encode a value for a URL template variable (RFC 8620 §1.5).
/// A blob's name is chosen by whoever sent the message; unencoded it
/// could carry a path, a query, or a line break into the request.
fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 0x0f] {
                let digit = char::from_digit(u32::from(nibble), 16).unwrap_or('0');
                out.push(digit.to_ascii_uppercase());
            }
        }
    }
    out
}

pub struct JmapClient {
    username: String,
    password: String,
    api_url: String,
    account_id: String,
    download_url: Option<String>,
    upload_url: Option<String>,
    /// The session's `maxSizeUpload`, when it names one.
    max_size_upload: Option<u64>,
    max_objects_in_set: usize,
    /// Whether the session lists mail submission for the account.
    submission: bool,
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
impl JmapClient {
    /// A client for a server that is not there, for the paths that need
    /// one to exist and not to answer.
    pub(crate) fn unreachable_for_tests() -> Self {
        JmapClient {
            username: "tester".to_string(),
            password: String::new(),
            api_url: "https://mail.invalid/jmap".to_string(),
            account_id: "account".to_string(),
            download_url: None,
            upload_url: None,
            max_size_upload: None,
            max_objects_in_set: FLAG_BATCH_MAX,
            submission: false,
        }
    }
}

#[derive(Debug)]
pub enum JmapError {
    Http(String),
    Parse(String),
    Api(String),
    /// No fetch service to carry the request. Its own text is the whole
    /// diagnostic, so it is printed without a category prefix.
    NoFetchService(String),
    /// The server may have done part of what was asked
    /// (`serverPartialFail`, RFC 8620 §3.6.2): what it did must be asked
    /// for before the request is made again.
    Unsettled(String),
}

impl std::fmt::Display for JmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JmapError::Http(e) => write!(f, "HTTP error: {}", e),
            JmapError::Parse(e) => write!(f, "Parse error: {}", e),
            JmapError::Api(e) => write!(f, "API error: {}", e),
            JmapError::NoFetchService(e) => write!(f, "{}", e),
            JmapError::Unsettled(e) => write!(f, "unsettled: {}", e),
        }
    }
}

impl JmapClient {
    fn auth_header(username: &str, password: &str) -> String {
        let credentials = format!("{}:{}", username, password);
        let encoded = td_encoding::base64(credentials.as_bytes());
        format!("Basic {}", encoded)
    }

    /// Fetch a URL following redirects manually while preserving the auth header.
    fn fetch_with_auth_following_redirects(
        url: &str,
        auth: &str,
        max_redirects: u32,
    ) -> Result<(String, String), JmapError> {
        let mut current_url = url.to_string();
        let mut carries = true;

        for i in 0..max_redirects {
            log_debug!("[JMAP] Request {} to: {}", i + 1, current_url);

            let fetched = get_with_auth(&current_url, carries.then_some(auth)).map_err(|e| {
                log_error!("[JMAP] Connection error: {}", e);
                e
            })?;
            let status = fetched.status;
            log_debug!("[JMAP] Got {} response", status);

            if (300..400).contains(&status) {
                match fetched.location {
                    Some(location) => {
                        log_debug!("[JMAP] Following redirect {} -> {}", status, location);
                        (current_url, carries) =
                            Self::redirect_target(url, &current_url, &location);
                        continue;
                    }
                    None => {
                        return Err(JmapError::Http(format!(
                            "Redirect {} without Location header",
                            status
                        )));
                    }
                }
            }

            if status >= 400 {
                let body = String::from_utf8_lossy(&fetched.body).into_owned();
                log_error!("[JMAP] HTTP error {}: {}", status, body);

                if matches!(status, 401 | 403) && !carries {
                    return Err(Self::left_the_origin(status, url, &current_url));
                }
                if status == 401 {
                    return Err(JmapError::Http(
                        "Authentication failed (401 Unauthorized)".to_string(),
                    ));
                }

                return Err(JmapError::Http(format!(
                    "HTTP {} error: {}",
                    status,
                    if body.is_empty() {
                        "(empty response)".to_string()
                    } else {
                        truncate_str(&body, 200).to_string()
                    }
                )));
            }

            let body = String::from_utf8(fetched.body)
                .map_err(|e| JmapError::Parse(format!("Failed to read response: {}", e)))?;

            if body.is_empty() {
                return Err(JmapError::Http(format!(
                    "Server returned empty response (status {})",
                    status
                )));
            }

            log_debug!("[JMAP] Response body length: {} bytes", body.len());
            return Ok((current_url, body));
        }

        Err(JmapError::Http("Too many redirects".to_string()))
    }

    /// Where a redirect goes, and whether the credential goes with it.
    /// The fetch service drops an `authorization` header on redirects;
    /// this client sends it again itself, so it is the client that
    /// decides, by the rule browsers and curl keep: the credential stays
    /// within the origin the request was configured for (`trusted`, the
    /// URL the loop started from), and a hop to any other origin — TLS
    /// or not — is followed bare. A `.well-known/jmap` that sends the
    /// client elsewhere and needs the credential there is configured by
    /// that URL directly.
    fn redirect_target(trusted: &str, base_url: &str, location: &str) -> (String, bool) {
        let target = Self::resolve_redirect(base_url, location);
        let carries = matches!(
            (Self::origin(&target), Self::origin(trusted)),
            (Some(a), Some(b)) if a == b
        );
        (target, carries)
    }

    /// A refusal from an origin the credential did not follow to.
    fn left_the_origin(status: u16, trusted: &str, current: &str) -> JmapError {
        JmapError::Http(format!(
            "HTTP {status} from {} after a redirect left {}: the credential does not \
             follow a redirect to another origin; configure that URL directly",
            Self::origin(current).unwrap_or_else(|| current.to_string()),
            Self::origin(trusted).unwrap_or_else(|| trusted.to_string()),
        ))
    }

    /// `scheme://host[:port]` of a URL (RFC 6454 §4): the part that says
    /// who receives the request. Lower-cased, since the scheme and the
    /// host are case-insensitive, and without the scheme's default port,
    /// which names the same receiver spelled or not.
    fn origin(url: &str) -> Option<String> {
        let scheme_end = url.find("://")?;
        let rest = url.get(scheme_end + 3..)?;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let mut origin = url.get(..scheme_end + 3 + end)?.to_ascii_lowercase();
        for (scheme, port) in [("http://", ":80"), ("https://", ":443")] {
            if origin.starts_with(scheme) && origin.ends_with(port) {
                origin.truncate(origin.len().saturating_sub(port.len()));
            }
        }
        Some(origin)
    }

    /// Resolve a redirect location against a base URL: absolute,
    /// scheme-relative (`//host/p`), root-relative (`/p`), or relative to
    /// the base's directory — where a base with no path (`https://host`)
    /// is that directory itself, not `https:/` with `host` for a file.
    fn resolve_redirect(base_url: &str, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            return location.to_string();
        }
        let Some(scheme_end) = base_url.find("://") else {
            return location.to_string();
        };
        if let Some(rest) = location.strip_prefix("//") {
            let scheme = base_url.get(..scheme_end).unwrap_or("https");
            return format!("{scheme}://{rest}");
        }
        // The query and fragment are not part of the directory.
        let base = base_url
            .find(['?', '#'])
            .and_then(|at| base_url.get(..at))
            .unwrap_or(base_url);
        let authority_start = scheme_end + 3;
        let path_start = base
            .get(authority_start..)
            .and_then(|rest| rest.find('/'))
            .map(|at| authority_start + at);
        let site = path_start.and_then(|at| base.get(..at)).unwrap_or(base);
        if location.starts_with('/') {
            return format!("{site}{location}");
        }
        let directory = path_start
            .and_then(|_| base.rfind('/'))
            .and_then(|last| base.get(..last))
            .unwrap_or(site);
        format!("{directory}/{location}")
    }

    pub fn discover(
        well_known_url: &str,
        username: &str,
        password: &str,
    ) -> Result<(JmapSession, Self), JmapError> {
        log_info!("[JMAP] Discovering JMAP session from: {}", well_known_url);
        let auth = Self::auth_header(username, password);

        let (_final_url, response_text) =
            Self::fetch_with_auth_following_redirects(well_known_url, &auth, 5)?;

        log_debug!("[JMAP] Session response received, parsing...");

        let session = td_json::parse(&response_text)
            .map_err(|e| e.to_string())
            .and_then(|value| JmapSession::from_json(&value))
            .map_err(|e| {
                JmapError::Parse(format!(
                    "Failed to parse session: {}. Response was: {}",
                    e,
                    truncate_str(&response_text, 500)
                ))
            })?;

        log_debug!("[JMAP] Session parsed, api_url: {}", session.api_url);

        let account_id = session
            .mail_account_id()
            .ok_or_else(|| {
                JmapError::Api(format!(
                    "No mail account found in session response: {}",
                    truncate_str(&response_text, 500)
                ))
            })?
            .to_string();

        log_info!("[JMAP] Discovery successful, account_id: {}", account_id);

        let submission = session.submits(&account_id);
        log_info!(
            "[JMAP] Mail submission for account {}: {}",
            account_id,
            if submission { "listed" } else { "not listed" }
        );
        let client = JmapClient {
            username: username.to_string(),
            password: password.to_string(),
            api_url: session.api_url.clone(),
            account_id,
            download_url: session.download_url.clone(),
            upload_url: session.upload_url.clone(),
            max_size_upload: session.max_size_upload,
            max_objects_in_set: session
                .max_objects_in_set
                .unwrap_or(FLAG_BATCH_MAX as u64)
                .clamp(1, FLAG_BATCH_MAX as u64) as usize,
            submission,
        };

        Ok((session, client))
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    fn call(&self, request: JmapRequest) -> Result<JmapResponse, JmapError> {
        if !td_fetch_client::available() {
            return Err(no_fetch_service());
        }
        let auth = Self::auth_header(&self.username, &self.password);

        // Writing a value cannot fail, so there is no serialize error to report.
        let request_json = request.to_json().to_string();
        log_debug!("[JMAP] Request body: {}", truncate_str(&request_json, 500));

        // The fetch service carries the request; a status of 400 or more is
        // an error here rather than a reply, as it was before.
        let send = || {
            td_fetch_client::post(
                &self.api_url,
                &[
                    ("authorization", auth.as_str()),
                    ("content-type", "application/json"),
                ],
                request_json.as_bytes(),
                RESPONSE_LIMIT,
            )
        };
        let response = if read_batch(&request) {
            read_backoff(send, std::thread::sleep)
        } else {
            send()
        }
        .map_err(|e| {
            log_error!("[JMAP] API call failed: {}", e);
            JmapError::Http(e.to_string())
        })?;
        if response.status >= 400 {
            log_error!("[JMAP] API call failed: status code {}", response.status);
            return Err(JmapError::Http(format!(
                "{}: status code {}",
                self.api_url, response.status
            )));
        }
        let response_text = String::from_utf8(response.body)
            .map_err(|e| JmapError::Parse(format!("Failed to read response: {}", e)))?;

        log_debug!(
            "[JMAP] Response body ({} bytes): {}",
            response_text.len(),
            truncate_str(&response_text, 1000)
        );

        let parsed = td_json::parse(&response_text)
            .map_err(|e| e.to_string())
            .and_then(|value| JmapResponse::from_json(&value))
            .map_err(|e| JmapError::Parse(format!("Failed to parse response: {}", e)))?;

        Ok(parsed)
    }

    pub fn get_mailboxes(&self) -> Result<Vec<Mailbox>, JmapError> {
        log_info!("[JMAP] Fetching mailboxes for account: {}", self.account_id);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Mailbox/get",
                json!({
                    "accountId": self.account_id,
                    "ids": null
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Mailbox/get" {
                let mailbox_response =
                    MailboxGetResponse::from_json(&method_response.1).map_err(JmapError::Parse)?;
                log_info!(
                    "[JMAP] Mailbox/get returned {} mailboxes",
                    mailbox_response.list.len()
                );
                return Ok(mailbox_response.list);
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    pub fn create_mailbox(&self, name: &str) -> Result<String, JmapError> {
        log_info!("[JMAP] Mailbox/set creating mailbox: {}", name);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Mailbox/set",
                json!({
                    "accountId": self.account_id,
                    "create": {
                        "newMailbox": {
                            "name": name
                        }
                    }
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        self.confirm_mailbox_creation(&response)
    }

    fn confirm_mailbox_creation(&self, response: &JmapResponse) -> Result<String, JmapError> {
        let reply = response
            .method_responses
            .first()
            .filter(|reply| reply.2 == "0")
            .ok_or_else(|| JmapError::Api("Unexpected response for Mailbox/set".into()))?;
        if reply.0 == "error" {
            return Err(JmapError::Api(format!("Mailbox/set refused: {}", reply.1)));
        }
        if reply.0 != "Mailbox/set" {
            return Err(JmapError::Api("Unexpected response for Mailbox/set".into()));
        }
        if reply.1.get("accountId").and_then(Json::as_str) != Some(self.account_id.as_str()) {
            return Err(JmapError::Api(
                "Mailbox/set returned a missing, invalid, or different account".into(),
            ));
        }
        if let Some(errors) = reply.1.get("notCreated") {
            if !errors.is_null() && errors.as_object().is_none() {
                return Err(JmapError::Api("Invalid Mailbox/set notCreated".into()));
            }
            if let Some(error) = errors.get("newMailbox") {
                return Err(JmapError::Api(format!("Mailbox creation refused: {error}")));
            }
        }
        if let Some(id) = reply
            .1
            .get("created")
            .and_then(|value| value.get("newMailbox"))
            .and_then(|value| value.get("id"))
            .and_then(Json::as_str)
            .filter(|id| valid_id(id))
        {
            Ok(id.to_string())
        } else {
            Err(JmapError::Api(
                "Mailbox creation did not return a created mailbox ID".into(),
            ))
        }
    }

    pub fn delete_mailbox(&self, id: &str) -> Result<(), JmapError> {
        log_info!("[JMAP] Mailbox/set deleting mailbox: {}", id);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Mailbox/set",
                json!({
                    "accountId": self.account_id,
                    "destroy": [id]
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        self.confirm_mailbox_deletion(&response, id)
    }

    fn confirm_mailbox_deletion(&self, response: &JmapResponse, id: &str) -> Result<(), JmapError> {
        let reply = response
            .method_responses
            .first()
            .filter(|reply| reply.2 == "0" && reply.0 == "Mailbox/set")
            .ok_or_else(|| JmapError::Api("Unexpected response for Mailbox/set destroy".into()))?;
        if reply.1.get("accountId").and_then(Json::as_str) != Some(self.account_id.as_str()) {
            return Err(JmapError::Api(
                "Mailbox/set destroy returned a missing, invalid, or different account".into(),
            ));
        }
        if let Some(errors) = reply.1.get("notDestroyed") {
            if !errors.is_null() && errors.as_object().is_none() {
                return Err(JmapError::Api("Invalid Mailbox/set notDestroyed".into()));
            }
            if let Some(error) = errors.get(id) {
                return Err(JmapError::Api(format!("Mailbox deletion refused: {error}")));
            }
        }
        let destroyed = reply.1.get("destroyed").and_then(Json::as_array);
        if !valid_id(id)
            || !destroyed
                .is_some_and(|ids| ids.len() == 1 && ids.first().and_then(Json::as_str) == Some(id))
        {
            return Err(JmapError::Api(
                "Mailbox deletion did not confirm the requested ID".into(),
            ));
        }
        Ok(())
    }

    pub fn query_emails(
        &self,
        mailbox_id: &str,
        limit: u32,
        position: u32,
        search_text: Option<&str>,
        received_after: Option<&str>,
        received_before: Option<&str>,
    ) -> Result<EmailQueryResult, JmapError> {
        log_info!(
            "[JMAP] Email/query for mailbox: {} (limit: {}, position: {}, search: {:?}, after: {:?}, before: {:?})",
            mailbox_id,
            limit,
            position,
            search_text,
            received_after,
            received_before
        );

        let mut conditions = vec![json!({ "inMailbox": mailbox_id })];
        if let Some(text) = search_text {
            conditions.push(json!({ "text": text }));
        }
        if let Some(after) = received_after {
            conditions.push(json!({ "after": after }));
        }
        if let Some(before) = received_before {
            conditions.push(json!({ "before": before }));
        }

        let filter = if conditions.len() == 1 {
            conditions
                .into_iter()
                .next()
                .unwrap_or_else(|| json!({ "inMailbox": mailbox_id }))
        } else {
            json!({
                "operator": "AND",
                "conditions": conditions
            })
        };

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/query",
                json!({
                    "accountId": self.account_id,
                    "filter": filter,
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "collapseThreads": false,
                    "limit": limit,
                    "position": position
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Email/query" {
                let query_response =
                    EmailQueryResponse::from_json(&method_response.1).map_err(JmapError::Parse)?;
                log_info!(
                    "[JMAP] Email/query returned {} email IDs (total: {:?})",
                    query_response.ids.len(),
                    query_response.total
                );
                return Ok(EmailQueryResult {
                    ids: query_response.ids,
                    total: query_response.total,
                    position: query_response.position,
                });
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    pub fn query_emails_uncollapsed(
        &self,
        mailbox_id: &str,
        limit: u32,
        position: u32,
    ) -> Result<EmailQueryResult, JmapError> {
        log_info!(
            "[JMAP] Email/query (uncollapsed) for mailbox: {} (limit: {}, position: {})",
            mailbox_id,
            limit,
            position
        );

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/query",
                json!({
                    "accountId": self.account_id,
                    "filter": { "inMailbox": mailbox_id },
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "collapseThreads": false,
                    "limit": limit,
                    "position": position
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Email/query" {
                let query_response =
                    EmailQueryResponse::from_json(&method_response.1).map_err(JmapError::Parse)?;
                return Ok(EmailQueryResult {
                    ids: query_response.ids,
                    total: query_response.total,
                    position: query_response.position,
                });
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    pub fn get_emails(&self, ids: &[String]) -> Result<Vec<Email>, JmapError> {
        self.get_emails_with_extra_properties(ids, &[])
    }

    pub fn get_emails_with_extra_properties(
        &self,
        ids: &[String],
        extra_properties: &[String],
    ) -> Result<Vec<Email>, JmapError> {
        let mut properties = vec![
            "id",
            "blobId",
            "threadId",
            "from",
            "to",
            "cc",
            "subject",
            "receivedAt",
            "preview",
            "textBody",
            "htmlBody",
            "bodyValues",
            "keywords",
            "mailboxIds",
            "attachments",
        ];
        properties.extend(extra_properties.iter().map(String::as_str));
        let mut emails = Vec::new();
        for chunk in ids.chunks(50) {
            if let Some(mut batch) = self.read_projection(chunk, &properties, true)? {
                emails.append(&mut batch);
                continue;
            }
            log_warn!(
                "[JMAP] Isolating interpretation refusal in {} messages",
                chunk.len()
            );
            for id in chunk {
                let one = std::slice::from_ref(id);
                if chunk.len() > 1 {
                    if let Some(mut batch) = self.read_projection(one, &properties, true)? {
                        emails.append(&mut batch);
                        continue;
                    }
                }
                let mut stored = self
                    .read_projection(
                        one,
                        &[
                            "id",
                            "blobId",
                            "threadId",
                            "receivedAt",
                            "keywords",
                            "mailboxIds",
                        ],
                        false,
                    )?
                    .ok_or_else(|| {
                        JmapError::Api(format!(
                            "Stored message metadata could not be read for {id}"
                        ))
                    })?;
                for email in &mut stored {
                    email.mark_unreadable();
                    log_warn!("[JMAP] Message {} has unreadable content", email.id);
                }
                emails.append(&mut stored);
            }
        }
        Ok(emails)
    }

    /// None is td-mta's deterministic interpretation refusal, never transport,
    /// storage, work-budget or authentication failure.
    fn read_projection(
        &self,
        ids: &[String],
        properties: &[&str],
        bodies: bool,
    ) -> Result<Option<Vec<Email>>, JmapError> {
        let response = self.call(JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/get",
                json!({
                    "accountId": self.account_id,
                    "ids": ids,
                    "properties": properties,
                    "fetchTextBodyValues": bodies,
                    "fetchHTMLBodyValues": bodies
                }),
                "0".to_string(),
            )],
        })?;
        let method = response
            .method_responses
            .first()
            .ok_or_else(|| JmapError::Api("Empty response for Email/get".into()))?;
        if method.0 == "Email/get" {
            return EmailGetResponse::from_json(&method.1)
                .map(|response| Some(response.list))
                .map_err(JmapError::Parse);
        }
        if method.0 == "error"
            && method.1.get("type").and_then(Json::as_str) == Some("serverFail")
            && method.1.get("description").and_then(Json::as_str) == Some("messageInterpretation")
        {
            return Ok(None);
        }
        Err(Self::method_error("Email/get", method))
    }

    pub fn get_emails_for_rules(
        &self,
        ids: &[String],
        extra_properties: &[String],
    ) -> Result<Vec<Email>, JmapError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        log_info!(
            "[JMAP] Email/get (rules) for {} email IDs with {} extra properties",
            ids.len(),
            extra_properties.len()
        );

        let mut properties = vec![
            "id",
            "threadId",
            "from",
            "to",
            "cc",
            "replyTo",
            "subject",
            "messageId",
            "receivedAt",
            "keywords",
            "mailboxIds",
        ];

        let extra_strs: Vec<&str> = extra_properties.iter().map(|s| s.as_str()).collect();
        properties.extend(extra_strs);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/get",
                json!({
                    "accountId": self.account_id,
                    "ids": ids,
                    "properties": properties
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Email/get" {
                let email_response =
                    EmailGetResponse::from_json(&method_response.1).map_err(JmapError::Parse)?;
                log_info!(
                    "[JMAP] Email/get (rules) returned {} emails",
                    email_response.list.len()
                );
                return Ok(email_response.list);
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    pub fn get_email(&self, id: &str) -> Result<Option<Email>, JmapError> {
        let emails = self.get_emails(&[id.to_string()])?;
        Ok(emails.into_iter().next())
    }

    pub fn get_email_for_reply(&self, id: &str) -> Result<Option<Email>, JmapError> {
        log_info!("[JMAP] Email/get for reply: {}", id);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/get",
                json!({
                    "accountId": self.account_id,
                    "ids": [id],
                    "properties": [
                        "id", "from", "to", "cc", "replyTo", "subject",
                        "receivedAt", "sentAt", "textBody", "htmlBody", "bodyValues",
                        "messageId", "references"
                    ],
                    "fetchTextBodyValues": true,
                    "fetchHTMLBodyValues": true
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Email/get" {
                let email_response =
                    EmailGetResponse::from_json(&method_response.1).map_err(JmapError::Parse)?;
                return Ok(email_response.list.into_iter().next());
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    fn validate_update_response<'a>(
        &self,
        response: &'a JmapResponse,
    ) -> Result<&'a Json, JmapError> {
        let reply = response
            .method_responses
            .first()
            .filter(|reply| reply.0 == "Email/set" && reply.2 == "0")
            .ok_or_else(|| {
                JmapError::Api(format!(
                    "Unexpected response for Email/set: {:?}",
                    response.method_responses.first()
                ))
            })?;
        if reply.1.get("accountId").and_then(Json::as_str) != Some(self.account_id.as_str()) {
            return Err(JmapError::Api(
                "Email/set returned a missing, invalid, or different account".into(),
            ));
        }
        for name in ["updated", "notUpdated"] {
            if reply
                .1
                .get(name)
                .is_some_and(|value| !value.is_null() && value.as_object().is_none())
            {
                return Err(JmapError::Api(format!("Invalid Email/set {name}")));
            }
        }
        Ok(&reply.1)
    }

    fn confirm_updates<'a>(
        &self,
        response: &JmapResponse,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), JmapError> {
        let reply = self.validate_update_response(response)?;
        Self::confirm_update_objects(reply, ids)
    }

    fn confirm_update_objects<'a>(
        reply: &Json,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), JmapError> {
        for id in ids {
            if let Some(error) = reply.get("notUpdated").and_then(|value| value.get(id)) {
                if error.get("type").and_then(Json::as_str) == Some("notFound") {
                    continue;
                }
                return Err(JmapError::Api(format!("Email/set refused {id}: {error}")));
            }
            let confirmed = reply
                .get("updated")
                .and_then(|value| value.get(id))
                .is_some_and(|value| value.is_null() || value.as_object().is_some());
            if !confirmed {
                return Err(JmapError::Api(format!("Email/set did not confirm {id}")));
            }
        }
        Ok(())
    }

    pub fn mark_emails_read(&self, ids: &[String]) -> Result<(), JmapError> {
        self.mark_emails_read_progress(ids, |_| Ok(()))
    }

    pub fn mark_emails_read_progress(
        &self,
        ids: &[String],
        mut confirmed: impl FnMut(&[String]) -> Result<(), String>,
    ) -> Result<(), JmapError> {
        let mut refused = None;
        log_info!("[JMAP] Email/set marking {} emails as read", ids.len());

        for chunk in ids.chunks(self.max_objects_in_set) {
            let mut update = td_json::ObjectBuilder::with_capacity(chunk.len());
            for id in chunk {
                update.insert(id.clone(), json!({ "keywords/$seen": true }));
            }
            let update = update.build();

            let request = JmapRequest {
                using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                method_calls: vec![MethodCall(
                    "Email/set",
                    json!({
                        "accountId": self.account_id,
                        "update": update
                    }),
                    "0".to_string(),
                )],
            };

            let response = self.call(request)?;

            let reply = self.validate_update_response(&response)?;
            let mut completed = Vec::new();
            for id in chunk {
                match Self::confirm_update_objects(reply, [id.as_str()]) {
                    Ok(()) => completed.push(id.clone()),
                    Err(error) => {
                        refused.get_or_insert(error);
                    }
                }
            }
            confirmed(&completed).map_err(JmapError::Api)?;
        }

        refused.map_or(Ok(()), Err)
    }

    pub fn mark_email_read(&self, id: &str) -> Result<(), JmapError> {
        log_info!("[JMAP] Email/set marking as read: {}", id);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/set",
                json!({
                    "accountId": self.account_id,
                    "update": {
                        id: {
                            "keywords/$seen": true
                        }
                    }
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        self.confirm_updates(&response, [id])
    }

    pub fn mark_email_unread(&self, id: &str) -> Result<(), JmapError> {
        log_info!("[JMAP] Email/set marking as unread: {}", id);

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/set",
                json!({
                    "accountId": self.account_id,
                    "update": {
                        id: {
                            "keywords/$seen": null
                        }
                    }
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        self.confirm_updates(&response, [id])
    }

    pub fn set_email_flagged(&self, id: &str, flagged: bool) -> Result<(), JmapError> {
        log_info!("[JMAP] Email/set flagged={} for: {}", flagged, id);

        let update_val = if flagged {
            json!({ "keywords/$flagged": true })
        } else {
            json!({ "keywords/$flagged": null })
        };

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/set",
                json!({
                    "accountId": self.account_id,
                    "update": {
                        id: update_val
                    }
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        self.confirm_updates(&response, [id])
    }

    pub fn move_email(&self, id: &str, to_mailbox_id: &str) -> Result<(), JmapError> {
        log_info!(
            "[JMAP] Email/set moving {} to mailbox {}",
            id,
            to_mailbox_id
        );

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/set",
                json!({
                    "accountId": self.account_id,
                    "update": {
                        id: {
                            "mailboxIds": { to_mailbox_id: true }
                        }
                    }
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        self.confirm_updates(&response, [id])
    }

    pub fn destroy_emails(&self, ids: &[String]) -> Result<(), JmapError> {
        if ids.is_empty() {
            return Ok(());
        }

        log_info!("[JMAP] Email/set destroying {} emails", ids.len());
        for chunk in ids.chunks(self.max_objects_in_set) {
            let request = JmapRequest {
                using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                method_calls: vec![MethodCall(
                    "Email/set",
                    json!({"accountId": self.account_id, "destroy": chunk}),
                    "0".to_string(),
                )],
            };
            let response = self.call(request)?;
            self.confirm_destructions(&response, chunk)?;
        }
        Ok(())
    }

    fn confirm_destructions(
        &self,
        response: &JmapResponse,
        ids: &[String],
    ) -> Result<(), JmapError> {
        let reply = response
            .method_responses
            .first()
            .filter(|reply| reply.0 == "Email/set" && reply.2 == "0")
            .ok_or_else(|| {
                JmapError::Api(format!(
                    "Unexpected response for Email/set destroy: {:?}",
                    response.method_responses.first()
                ))
            })?;
        if reply.1.get("accountId").and_then(Json::as_str) != Some(self.account_id.as_str()) {
            return Err(JmapError::Api(
                "Email/set destroy returned a missing, invalid, or different account".into(),
            ));
        }
        let destroyed = match reply.1.get("destroyed") {
            None | Some(Json::Null) => &[][..],
            Some(value) => value
                .as_array()
                .ok_or_else(|| JmapError::Api("Invalid Email/set destroyed".into()))?,
        };
        if destroyed.iter().any(|value| value.as_str().is_none()) {
            return Err(JmapError::Api("Invalid Email/set destroyed ID".into()));
        }
        let refused = reply.1.get("notDestroyed");
        if refused.is_some_and(|value| !value.is_null() && value.as_object().is_none()) {
            return Err(JmapError::Api("Invalid Email/set notDestroyed".into()));
        }
        for id in ids {
            let confirmed = destroyed
                .iter()
                .any(|value| value.as_str() == Some(id.as_str()));
            let error = refused.and_then(|value| value.get(id));
            match (confirmed, error) {
                (true, None) => (),
                (false, Some(error))
                    if error.get("type").and_then(Json::as_str) == Some("notFound") => {}
                (_, Some(error)) => {
                    return Err(JmapError::Api(format!(
                        "Email/set destroy refused {id}: {error}"
                    )))
                }
                _ => {
                    return Err(JmapError::Api(format!(
                        "Email/set destroy did not confirm {id}"
                    )))
                }
            }
        }
        Ok(())
    }

    // ---- submission ------------------------------------------------------

    /// Whether the session lists mail submission for this account.
    pub fn can_submit(&self) -> bool {
        self.submission
    }

    /// The most bytes one upload may carry: the server's ceiling when the
    /// session names one, and the fetch service's request bound always.
    pub fn upload_ceiling(&self) -> u64 {
        self.max_size_upload
            .map_or(td_fetch_client::MAX_REQUEST_BODY, |max| {
                max.min(td_fetch_client::MAX_REQUEST_BODY)
            })
    }

    /// The account's identities, `Identity/get` with every id.
    pub fn get_identities(&self) -> Result<Vec<Identity>, JmapError> {
        log_info!("[JMAP] Identity/get for account: {}", self.account_id);
        let request = JmapRequest {
            using: vec![
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:submission",
            ],
            method_calls: vec![MethodCall(
                "Identity/get",
                json!({
                    "accountId": self.account_id,
                    "ids": Json::Null
                }),
                "0".to_string(),
            )],
        };
        let response = self.call(request)?;
        let Some(method_response) = response.method_responses.first() else {
            return Err(JmapError::Api(
                "Empty response for Identity/get".to_string(),
            ));
        };
        if method_response.0 != "Identity/get" {
            return Err(Self::method_error("Identity/get", method_response));
        }
        let list = method_response
            .1
            .get("list")
            .and_then(|list| list.as_array())
            .ok_or_else(|| JmapError::Parse("Identity/get has no list".to_string()))?;
        let mut identities = Vec::with_capacity(list.len());
        for value in list {
            identities.push(Identity::from_json(value).map_err(JmapError::Parse)?);
        }
        Ok(identities)
    }

    /// Uploads `bytes` as a blob of `content_type` at the session's upload
    /// URL and answers its blob id.
    pub fn upload_blob(&self, bytes: &[u8], content_type: &str) -> Result<String, JmapError> {
        if !td_fetch_client::available() {
            return Err(no_fetch_service());
        }
        let Some(upload_url) = &self.upload_url else {
            return Err(JmapError::Api(
                "the session names no upload URL, so nothing can be attached".to_string(),
            ));
        };
        let url = upload_url.replace("{accountId}", &uri_encode(&self.account_id));
        log_info!(
            "[JMAP] Uploading {} bytes of {} to {}",
            bytes.len(),
            content_type,
            url
        );
        let auth = Self::auth_header(&self.username, &self.password);
        let response = td_fetch_client::post(
            &url,
            &[
                ("authorization", auth.as_str()),
                ("content-type", content_type),
            ],
            bytes,
            RESPONSE_LIMIT,
        )
        .map_err(|e| JmapError::Http(e.to_string()))?;
        if response.status >= 400 {
            return Err(JmapError::Http(format!(
                "{}: status code {}",
                url, response.status
            )));
        }
        let text = String::from_utf8(response.body)
            .map_err(|e| JmapError::Parse(format!("Failed to read the upload's reply: {e}")))?;
        let value = td_json::parse(&text)
            .map_err(|e| JmapError::Parse(format!("Failed to parse the upload's reply: {e}")))?;
        value
            .get("blobId")
            .and_then(|id| id.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                JmapError::Parse(format!(
                    "the upload's reply names no blobId: {}",
                    truncate_str(&text, 200)
                ))
            })
    }

    /// Creates the message and submits it in one request: `Email/set`
    /// creates it in `mailbox_id` with `$draft` and `$seen`, and
    /// `EmailSubmission/set` sends it under `identity_id`, on success
    /// dropping `$draft` and, when `sent_mailbox_id` is another mailbox,
    /// moving it there. Answers the email's id and the submission's, and
    /// whether that filing was done: its implicit `Email/set` answer is
    /// read, so a message sent but left a draft is said to be. A refused
    /// submission has the message the server made destroyed, best
    /// effort, since the file is the draft of record.
    pub fn submit_email(
        &self,
        email: Json,
        identity_id: &str,
        mailbox_id: &str,
        sent_mailbox_id: Option<&str>,
    ) -> Result<Submitted, JmapError> {
        let mut email = email;
        if let Json::Obj(pairs) = &mut email {
            pairs.push(("mailboxIds".to_string(), json!({ mailbox_id: true })));
            pairs.push((
                "keywords".to_string(),
                json!({ "$draft": true, "$seen": true }),
            ));
        }
        let mut on_success = ObjectBuilder::new().set("keywords/$draft", Json::Null);
        if let Some(sent) = sent_mailbox_id.filter(|sent| *sent != mailbox_id) {
            on_success = on_success
                .set(format!("mailboxIds/{sent}"), true)
                .set(format!("mailboxIds/{mailbox_id}"), Json::Null);
        }
        log_info!(
            "[JMAP] Email/set create and EmailSubmission/set for identity {}",
            identity_id
        );
        let request = JmapRequest {
            using: vec![
                "urn:ietf:params:jmap:core",
                "urn:ietf:params:jmap:mail",
                "urn:ietf:params:jmap:submission",
            ],
            method_calls: vec![
                MethodCall(
                    "Email/set",
                    json!({
                        "accountId": self.account_id,
                        "create": { "draft": email }
                    }),
                    "0".to_string(),
                ),
                MethodCall(
                    "EmailSubmission/set",
                    json!({
                        "accountId": self.account_id,
                        "create": {
                            "submission": {
                                "emailId": "#draft",
                                "identityId": identity_id
                            }
                        },
                        "onSuccessUpdateEmail": { "#submission": on_success.build() }
                    }),
                    "1".to_string(),
                ),
            ],
        };
        let response = self.call(request)?;

        let mut email_id = None;
        let mut submission_id = None;
        let mut filed = Ok(());
        for method_response in &response.method_responses {
            match method_response.0.as_str() {
                "Email/set" if method_response.2 == "1" => {
                    // The filing on success, updating the created message.
                    let not_updated = method_response
                        .1
                        .get("notUpdated")
                        .and_then(|n| n.as_object())
                        .filter(|n| !n.is_empty());
                    if let Some(not_updated) = not_updated {
                        let refusal = email_id
                            .as_deref()
                            .and_then(|id| not_updated.iter().find(|(key, _)| key == id))
                            .or_else(|| not_updated.first())
                            .map(|(_, error)| Self::set_error_text(error))
                            .unwrap_or_else(|| "refused".to_string());
                        filed = Err(refusal);
                    }
                }
                "Email/set" if method_response.2 == "0" => {
                    if let Some(refusal) = method_response.1.get_path(&["notCreated", "draft"]) {
                        return Err(JmapError::Api(format!(
                            "the server refused the message: {}",
                            Self::set_error_text(refusal)
                        )));
                    }
                    email_id = method_response
                        .1
                        .get_path(&["created", "draft", "id"])
                        .and_then(|id| id.as_str())
                        .map(str::to_string);
                }
                "EmailSubmission/set" => {
                    if let Some(refusal) = method_response.1.get_path(&["notCreated", "submission"])
                    {
                        let cleanup = self.cleanup_created(email_id.as_ref());
                        return Err(JmapError::Api(format!(
                            "the server refused to send: {}{cleanup}",
                            Self::set_error_text(refusal)
                        )));
                    }
                    submission_id = method_response
                        .1
                        .get_path(&["created", "submission", "id"])
                        .and_then(|id| id.as_str())
                        .map(str::to_string);
                }
                "error" => {
                    // The call id says whose error it is: the implicit
                    // Email/set of the filing answers under the
                    // submission's id, after the submission is created.
                    match method_response.2.as_str() {
                        "0" => return Err(Self::method_error("Email/set", method_response)),
                        _ if submission_id.is_none() => {
                            let error = Self::method_error("EmailSubmission/set", method_response);
                            // A partial failure may have sent: nothing
                            // is removed until the server is asked.
                            let kind = method_response.1.get("type").and_then(|t| t.as_str());
                            if kind == Some("serverPartialFail") {
                                return Err(JmapError::Unsettled(error.to_string()));
                            }
                            let cleanup = self.cleanup_created(email_id.as_ref());
                            return Err(JmapError::Api(format!("{error}{cleanup}")));
                        }
                        _ => {
                            filed =
                                Err(Self::method_error("the filing", method_response).to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        match (email_id, submission_id) {
            (Some(email_id), Some(submission_id)) => Ok(Submitted {
                email_id,
                submission_id,
                filed,
            }),
            (None, _) => Err(JmapError::Api(
                "the server created no message for the draft".to_string(),
            )),
            (Some(_), None) => Err(JmapError::Api(
                "the server created the message but reported no submission".to_string(),
            )),
        }
    }

    /// The copy `Email/set` made of a message whose submission was not
    /// created is removed again, best effort: the draft of record is the
    /// file. What to append to the refusal, nothing when nothing was made
    /// or the copy went.
    fn cleanup_created(&self, email_id: Option<&String>) -> String {
        match email_id {
            Some(id) => match self.destroy_emails(std::slice::from_ref(id)) {
                Ok(()) => String::new(),
                Err(e) => format!("; the copy the server made, {id}, could not be removed: {e}"),
            },
            None => String::new(),
        }
    }

    /// The submissions the identity made between `since` and `until`
    /// (RFC 3339), each with the id of the message it sent:
    /// `EmailSubmission/query` by identity and time, then
    /// `EmailSubmission/get`. The way a send whose answer was lost is
    /// found again; a query by the Message-ID header would name it
    /// directly, but servers answer that filter as they please
    /// (Stalwart, for one, matches nothing), and every server answers
    /// time.
    pub fn submissions_between(
        &self,
        identity_id: &str,
        since: &str,
        until: &str,
    ) -> Result<Vec<(String, String)>, JmapError> {
        log_info!(
            "[JMAP] EmailSubmission/query for identity {} between {} and {}",
            identity_id,
            since,
            until
        );
        let using = vec![
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:submission",
        ];
        let mut ids = Vec::new();
        let mut state = None;
        let mut total = None;
        loop {
            let response = self.call(JmapRequest {
                using: using.clone(),
                method_calls: vec![MethodCall(
                    "EmailSubmission/query",
                    json!({
                        "accountId": self.account_id,
                        "filter": {"identityIds": [identity_id], "after": since, "before": until},
                        "position": ids.len(), "limit": 256, "calculateTotal": true,
                        "sort": [{"property": "sendAt", "isAscending": true}]
                    }),
                    "0".into(),
                )],
            })?;
            let body = self.reconciliation_response(&response, "EmailSubmission/query")?;
            if reconciliation_page(body, &mut ids, &mut state, &mut total)? {
                break;
            }
        }
        let mut submissions = Vec::new();
        for batch in ids.chunks(50) {
            let response = self.call(JmapRequest {
                using: using.clone(),
                method_calls: vec![MethodCall(
                    "EmailSubmission/get",
                    json!({
                        "accountId": self.account_id, "ids": batch,
                        "properties": ["id", "emailId"]
                    }),
                    "0".into(),
                )],
            })?;
            let body = self.reconciliation_response(&response, "EmailSubmission/get")?;
            submissions.extend(reconciliation_submissions(body, batch)?);
        }
        Ok(submissions)
    }

    /// Complete the recorded folder's bounded recovery window before absence
    /// can authorize a retry. Ordinary mailbox browsing may remain paginated.
    pub fn filing_between(
        &self,
        mailbox: &str,
        since: &str,
        until: &str,
    ) -> Result<Vec<Email>, JmapError> {
        let mut ids = Vec::new();
        let mut state = None;
        let mut total = None;
        loop {
            let response = self.call(JmapRequest {
                using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                method_calls: vec![MethodCall(
                    "Email/query",
                    json!({
                        "accountId": self.account_id,
                        "filter": {"operator":"AND", "conditions":[
                            {"inMailbox":mailbox}, {"after":since}, {"before":until}
                        ]},
                        "sort":[{"property":"receivedAt", "isAscending":false}],
                        "collapseThreads":false,
                        "position":ids.len(), "limit":256, "calculateTotal":true
                    }),
                    "0".into(),
                )],
            })?;
            let body = self.reconciliation_response(&response, "Email/query")?;
            if reconciliation_page(body, &mut ids, &mut state, &mut total)? {
                break;
            }
        }
        self.get_email_filing(&ids)
    }

    /// The messages' filing: keywords, mailboxes and Message-IDs.
    pub fn get_email_filing(&self, ids: &[String]) -> Result<Vec<Email>, JmapError> {
        if ids.len() > RECONCILIATION_MAX || ids.iter().any(|id| !valid_id(id)) {
            return Err(reconciliation_incomplete());
        }
        let ids: std::collections::BTreeSet<&String> = ids.iter().collect();
        let ids: Vec<&String> = ids.into_iter().collect();
        let mut emails = Vec::new();
        for batch in ids.chunks(50) {
            let response = self.call(JmapRequest {
                using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                method_calls: vec![MethodCall(
                    "Email/get",
                    json!({
                        "accountId": self.account_id, "ids": batch,
                        "properties": ["id", "keywords", "mailboxIds", "messageId"]
                    }),
                    "0".into(),
                )],
            })?;
            let body = self.reconciliation_response(&response, "Email/get")?;
            reconciliation_filing_fields(body)?;
            let result = EmailGetResponse::from_json(body).map_err(JmapError::Parse)?;
            let mut remaining: std::collections::BTreeSet<&str> =
                batch.iter().map(|id| id.as_str()).collect();
            if !result.not_found.is_empty() || result.list.len() != batch.len() {
                return Err(reconciliation_incomplete());
            }
            for email in result.list {
                if !remaining.remove(email.id.as_str()) {
                    return Err(reconciliation_incomplete());
                }
                emails.push(email);
            }
        }
        Ok(emails)
    }

    fn reconciliation_response<'a>(
        &self,
        response: &'a JmapResponse,
        method: &str,
    ) -> Result<&'a Json, JmapError> {
        let [response] = response.method_responses.as_slice() else {
            return Err(reconciliation_incomplete());
        };
        if response.0 != method {
            return Err(Self::method_error(method, response));
        }
        if response.2 != "0"
            || response.1.get("accountId").and_then(Json::as_str) != Some(self.account_id.as_str())
        {
            return Err(reconciliation_incomplete());
        }
        Ok(&response.1)
    }

    /// A method-level `error` response, or a response of the wrong name,
    /// as one line.
    fn method_error(method: &str, response: &MethodResponse) -> JmapError {
        if response.0 == "error" {
            let kind = response
                .1
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown");
            let description = response
                .1
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            JmapError::Api(
                format!("{method} failed: {kind} {description}")
                    .trim_end()
                    .to_string(),
            )
        } else {
            JmapError::Api(format!("Unexpected response for {method}: {}", response.0))
        }
    }

    /// A `SetError`'s type, description and properties as one line.
    fn set_error_text(error: &Json) -> String {
        let kind = error
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown");
        let description = error
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        let properties = error
            .get("properties")
            .and_then(|p| p.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|p| !p.is_empty());
        let mut text = kind.to_string();
        if !description.is_empty() {
            text.push_str(": ");
            text.push_str(description);
        }
        if let Some(properties) = properties {
            text.push_str(" (");
            text.push_str(&properties);
            text.push(')');
        }
        text
    }

    pub fn download_blob(
        &self,
        blob_id: &str,
        name: &str,
        content_type: &str,
    ) -> Result<Vec<u8>, JmapError> {
        let download_url = match &self.download_url {
            Some(url) => url,
            None => {
                return Err(JmapError::Api("No download URL available".to_string()));
            }
        };

        let url = download_url
            .replace("{accountId}", &uri_encode(&self.account_id))
            .replace("{blobId}", &uri_encode(blob_id))
            .replace("{name}", &uri_encode(name))
            .replace("{type}", &uri_encode(content_type));

        log_debug!("[JMAP] Downloading blob from: {}", url);

        let auth = Self::auth_header(&self.username, &self.password);

        let mut current_url = url.clone();
        let mut carries = true;
        for _ in 0..5 {
            let fetched = get_with_auth(&current_url, carries.then_some(auth.as_str()))?;
            let status = fetched.status;
            if (300..400).contains(&status) {
                match fetched.location {
                    Some(location) => {
                        (current_url, carries) =
                            Self::redirect_target(&url, &current_url, &location);
                        continue;
                    }
                    None => {
                        return Err(JmapError::Http(format!(
                            "Redirect {} without Location header",
                            status
                        )));
                    }
                }
            }
            if matches!(status, 401 | 403) && !carries {
                return Err(Self::left_the_origin(status, &url, &current_url));
            }
            if status >= 400 {
                return Err(JmapError::Http(format!("HTTP {} error", status)));
            }

            log_info!("[JMAP] Blob downloaded, {} bytes", fetched.body.len());
            return Ok(fetched.body);
        }

        Err(JmapError::Http("Too many redirects".to_string()))
    }

    pub fn get_email_keywords(&self, ids: &[String]) -> Result<Vec<Email>, JmapError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        log_info!("[JMAP] Email/get keywords for {} email IDs", ids.len());

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/get",
                json!({
                    "accountId": self.account_id,
                    "ids": ids,
                    "properties": ["id", "keywords", "mailboxIds"]
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Email/get" {
                let email_response =
                    EmailGetResponse::from_json(&method_response.1).map_err(JmapError::Parse)?;
                log_info!(
                    "[JMAP] Email/get keywords returned {} emails",
                    email_response.list.len()
                );
                return Ok(email_response.list);
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    pub fn get_threads(&self, ids: &[String]) -> Result<Vec<super::types::Thread>, JmapError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        log_info!("[JMAP] Thread/get for {} thread IDs", ids.len());

        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Thread/get",
                json!({
                    "accountId": self.account_id,
                    "ids": ids
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Thread/get" {
                let thread_response =
                    super::types::ThreadGetResponse::from_json(&method_response.1)
                        .map_err(JmapError::Parse)?;
                log_info!(
                    "[JMAP] Thread/get returned {} threads",
                    thread_response.list.len()
                );
                return Ok(thread_response.list);
            }
        }

        Err(JmapError::Api("Unexpected response".to_string()))
    }

    pub fn query_thread_emails(&self, thread_id: &str) -> Result<Vec<Email>, JmapError> {
        let threads = self.get_threads(&[thread_id.to_string()])?;
        let Some(thread) = threads.into_iter().find(|thread| thread.id == thread_id) else {
            return Ok(Vec::new());
        };
        self.get_emails(&thread.email_ids)
    }

    pub fn get_email_raw(&self, id: &str) -> Result<Option<String>, JmapError> {
        log_info!("[JMAP] Fetching raw email via blob: {}", id);

        // First, get the blobId for this email
        let request = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            method_calls: vec![MethodCall(
                "Email/get",
                json!({
                    "accountId": self.account_id,
                    "ids": [id],
                    "properties": ["id", "blobId"]
                }),
                "0".to_string(),
            )],
        };

        let response = self.call(request)?;

        let blob_id = if let Some(method_response) = response.method_responses.first() {
            if method_response.0 == "Email/get" {
                method_response
                    .1
                    .get("list")
                    .and_then(|list| list.as_array())
                    .and_then(|list| list.first())
                    .and_then(|email| email.get("blobId"))
                    .and_then(|blob| blob.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        } else {
            None
        };

        let blob_id = match blob_id {
            Some(id) => id,
            None => {
                log_error!("[JMAP] No blobId found for email: {}", id);
                return Ok(None);
            }
        };

        let download_url = match &self.download_url {
            Some(url) => url,
            None => {
                return Err(JmapError::Api("No download URL available".to_string()));
            }
        };

        let url = download_url
            .replace("{accountId}", &uri_encode(&self.account_id))
            .replace("{blobId}", &uri_encode(&blob_id))
            .replace("{name}", &uri_encode("email.eml"))
            .replace("{type}", &uri_encode("message/rfc822"));

        log_debug!("[JMAP] Downloading blob from: {}", url);

        let auth = Self::auth_header(&self.username, &self.password);
        let (_, body) = Self::fetch_with_auth_following_redirects(&url, &auth, 5)?;

        log_info!("[JMAP] Raw email downloaded, {} bytes", body.len());
        Ok(Some(body))
    }
}

fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        // Find a valid UTF-8 boundary
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_filing_requires_requested_properties() {
        for good in [
            json!({"list":[{"keywords":{}, "mailboxIds":{"inbox":true}, "messageId":null}]}),
            json!({"list":[{"keywords":{"$draft":true}, "mailboxIds":{"inbox":true}, "messageId":["id@example.com"]}]}),
        ] {
            assert!(reconciliation_filing_fields(&good).is_ok());
        }
        for bad in [
            json!({"list":[{"keywords":{}, "mailboxIds":{}}]}),
            json!({"list":[{"keywords":{}, "messageId":null}]}),
            json!({"list":[{"mailboxIds":{}, "messageId":null}]}),
            json!({"list":[{"keywords":{}, "mailboxIds":{}, "messageId":[false]}]}),
            json!({"list":[{"keywords":{"$draft":false}, "mailboxIds":{}, "messageId":null}]}),
        ] {
            assert!(reconciliation_filing_fields(&bad).is_err());
        }
    }

    #[test]
    fn reconciliation_requires_complete_stable_pages_and_gets() {
        let mut ids = Vec::new();
        let mut state = None;
        let mut total = None;
        let first: Vec<String> = (0..256).map(|n| format!("s{n}")).collect();
        assert!(!reconciliation_page(
            &json!({"queryState":"a", "position":0, "total":257, "ids":first}),
            &mut ids,
            &mut state,
            &mut total
        )
        .unwrap());
        for bad in [
            json!({"queryState":"b", "position":256, "total":257, "ids":["last"]}),
            json!({"queryState":"a", "position":0, "total":257, "ids":["last"]}),
            json!({"queryState":"a", "position":256, "total":258, "ids":["last"]}),
            json!({"queryState":"a", "position":256, "total":257, "ids":[]}),
            json!({"queryState":"a", "position":256, "total":257, "ids":["s0"]}),
            json!({"queryState":"a", "position":256, "total":257, "ids":[null]}),
        ] {
            assert!(
                reconciliation_page(&bad, &mut ids.clone(), &mut state.clone(), &mut total)
                    .is_err()
            );
        }
        assert!(reconciliation_page(
            &json!({"queryState":"a", "position":256, "total":257, "ids":["last"]}),
            &mut ids,
            &mut state,
            &mut total
        )
        .unwrap());
        assert_eq!(ids.last().map(String::as_str), Some("last"));
        let wanted = vec!["last".into()];
        assert!(reconciliation_submissions(
            &json!({"list":[{"id":"last","emailId":"email"}],"notFound":[]}),
            &wanted
        )
        .is_ok());
        for bad in [
            json!({}),
            json!({"list":[],"notFound":["last"]}),
            json!({"list":[{"id":"other","emailId":"email"}],"notFound":[]}),
            json!({"list":[{"id":"last"}],"notFound":[]}),
        ] {
            assert!(reconciliation_submissions(&bad, &wanted).is_err());
        }
    }

    #[test]
    fn destruction_requires_typed_confirmation_for_every_requested_id() {
        let client = JmapClient::unreachable_for_tests();
        let response = |method: &str, call: &str, body| JmapResponse {
            method_responses: vec![MethodResponse(method.into(), body, call.into())],
            session_state: "state".into(),
        };
        let ids = ["a".to_string(), "b".to_string()];
        let good = json!({"accountId":"account","destroyed":["a"],"notDestroyed":{"b":{"type":"notFound"}}});
        assert!(client
            .confirm_destructions(&response("Email/set", "0", good.clone()), &ids)
            .is_ok());
        assert!(client
            .confirm_destructions(
                &response("error", "0", json!({"type":"accountNotFound"})),
                &ids
            )
            .is_err());
        assert!(client
            .confirm_destructions(&response("Email/set", "other", good), &ids)
            .is_err());
        for body in [
            json!({"accountId":"wrong","destroyed":["a","b"]}),
            json!({"destroyed":["a","b"]}),
            json!({"accountId":"account","destroyed":["a"]}),
            json!({"accountId":"account","destroyed":["a","b"],"notDestroyed":{"b":{"type":"notFound"}}}),
            json!({"accountId":"account","destroyed":["a",true]}),
            json!({"accountId":"account","destroyed":{}}),
            json!({"accountId":"account","notDestroyed":[]}),
            json!({"accountId":"account","notDestroyed":{"a":{"type":"accountNotFound"},"b":{"type":"notFound"}}}),
        ] {
            assert!(
                client
                    .confirm_destructions(&response("Email/set", "0", body.clone()), &ids)
                    .is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn mailbox_deletion_requires_explicit_matching_confirmation() {
        let client = JmapClient::unreachable_for_tests();
        let response = |method: &str, call: &str, body| JmapResponse {
            method_responses: vec![MethodResponse(method.into(), body, call.into())],
            session_state: "state".into(),
        };
        for errors in [Json::Null, json!({})] {
            assert!(client
                .confirm_mailbox_deletion(
                    &response(
                        "Mailbox/set",
                        "0",
                        json!({"accountId":"account", "destroyed":["box"], "notDestroyed":errors})
                    ),
                    "box"
                )
                .is_ok());
        }
        let success = json!({"accountId":"account", "destroyed":["box"]});
        for (method, call) in [("Mailbox/set", "other"), ("Email/set", "0"), ("error", "0")] {
            assert!(client
                .confirm_mailbox_deletion(&response(method, call, success.clone()), "box")
                .is_err());
        }
        for body in [
            json!({"accountId":"account"}),
            json!({"destroyed":["box"]}),
            json!({"accountId":"other", "destroyed":["box"]}),
            json!({"accountId":"account", "destroyed":null}),
            json!({"accountId":"account", "destroyed":[]}),
            json!({"accountId":"account", "destroyed":{}}),
            json!({"accountId":"account", "destroyed":[true]}),
            json!({"accountId":"account", "destroyed":["other"]}),
            json!({"accountId":"account", "destroyed":["box", "box"]}),
            json!({"accountId":"account", "destroyed":["box"], "notDestroyed":[]}),
            json!({"accountId":"account", "destroyed":["box"], "notDestroyed":{"box":{"type":"notFound"}}}),
            json!({"accountId":"account", "notDestroyed":{"box":{"type":"mailboxHasEmail"}}}),
        ] {
            assert!(
                client
                    .confirm_mailbox_deletion(&response("Mailbox/set", "0", body.clone()), "box")
                    .is_err(),
                "{body}"
            );
        }
        assert!(client
            .confirm_mailbox_deletion(
                &JmapResponse {
                    method_responses: Vec::new(),
                    session_state: "state".into(),
                },
                "box"
            )
            .is_err());
    }

    #[test]
    fn mailbox_creation_requires_account_call_and_created_id_confirmation() {
        let client = JmapClient::unreachable_for_tests();
        let response = |method: &str, call: &str, body| JmapResponse {
            method_responses: vec![MethodResponse(method.into(), body, call.into())],
            session_state: "state".into(),
        };
        let success = json!({"accountId":"account", "created":{"newMailbox":{"id":"mailbox"}}});
        assert!(client
            .confirm_mailbox_creation(&response("Mailbox/set", "0", success.clone()))
            .is_ok());
        assert!(client
            .confirm_mailbox_creation(&response("Mailbox/set", "other", success.clone()))
            .is_err());
        assert!(client
            .confirm_mailbox_creation(&response("error", "0", success))
            .is_err());
        for body in [
            json!({"accountId":"other", "created":{"newMailbox":{"id":"mailbox"}}}),
            json!({"created":{"newMailbox":{"id":"mailbox"}}}),
            json!({"accountId":"account", "created":{"newMailbox":{}}}),
            json!({"accountId":"account", "created":{"newMailbox":{"id":""}}}),
            json!({"accountId":"account", "created":{"newMailbox":{"id":"bad\nId"}}}),
            json!({"accountId":"account", "created":{"newMailbox":{"id":"x".repeat(256)}}}),
            json!({"accountId":"account", "created":{"newMailbox":{"id":"mailbox"}}, "notCreated":{"newMailbox":{"type":"invalidProperties"}}}),
            json!({"accountId":"account", "created":{"newMailbox":{"id":"mailbox"}}, "notCreated":[]}),
            json!({"accountId":"account"}),
        ] {
            assert!(
                client
                    .confirm_mailbox_creation(&response("Mailbox/set", "0", body.clone()))
                    .is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn flag_success_requires_every_requested_update() {
        let client = JmapClient::unreachable_for_tests();
        let response = |name: &str, call: &str, body| JmapResponse {
            method_responses: vec![MethodResponse(name.into(), body, call.into())],
            session_state: "state".into(),
        };
        let success =
            json!({"accountId":"account", "updated":{"a":null,"b":{}}, "notUpdated":null});
        assert!(client
            .confirm_updates(&response("Email/set", "0", success.clone()), ["a", "b"])
            .is_ok());
        assert!(client
            .confirm_updates(&response("Email/set", "other", success.clone()), ["a"])
            .is_err());
        assert!(client
            .confirm_updates(&response("error", "0", success), ["a"])
            .is_err());
        for body in [
            json!({"accountId":"account", "updated":{"a":null}}),
            json!({"accountId":"account", "updated":{"a":null,"b":null}, "notUpdated":{"b":{"type":"forbidden"}}}),
            json!({"accountId":"account", "updated":{"a":null,"b":false}}),
            json!({"accountId":"other", "updated":{"a":null,"b":null}}),
            json!({"accountId":"account", "updated":null}),
            json!({"accountId":"account", "updated":{"a":null,"b":null}, "notUpdated":[]}),
            json!({"accountId":"account"}),
        ] {
            assert!(
                client
                    .confirm_updates(&response("Email/set", "0", body.clone()), ["a", "b"])
                    .is_err(),
                "{body}"
            );
        }
        assert!(client
            .confirm_updates(
                &response(
                    "Email/set",
                    "0",
                    json!({"accountId":"account", "notUpdated":{"a":{"type":"notFound"}}})
                ),
                ["a"]
            )
            .is_ok());
        assert!(client.mark_emails_read(&[]).is_ok());
    }

    #[test]
    fn read_rate_limit_delays_are_bounded_and_other_failures_are_not_replayed() {
        use td_fetch_client::{Error, Response};
        for (status, delay, attempts) in [
            (429, "1", 3),
            (429, "5", 3),
            (429, "0", 1),
            (429, "6", 1),
            (429, "999999999999999999999", 1),
            (429, "Wed, 21 Oct 2015 07:28:00 GMT", 1),
            (429, "+1", 1),
            (429, "", 1),
            (401, "1", 1),
            (503, "1", 3),
            (503, "6", 1),
            (503, "", 1),
            (200, "1", 1),
        ] {
            let mut calls = 0;
            let mut waits = Vec::new();
            let reply = read_backoff(
                || {
                    calls += 1;
                    Ok(Response {
                        status,
                        headers: vec![("retry-after".into(), delay.into())],
                        body: vec![42],
                    })
                },
                |duration| waits.push(duration),
            )
            .unwrap();
            assert_eq!(calls, attempts);
            assert_eq!(waits.len(), attempts - 1);
            assert_eq!(reply.status, status);
            assert_eq!(reply.body, [42]);
            assert!(waits
                .iter()
                .all(|d| d.as_secs() == delay.parse::<u64>().unwrap()));
        }
        let mut calls = 0;
        let mut waits = Vec::new();
        let reply = read_backoff(
            || {
                calls += 1;
                Ok(Response {
                    status: 503,
                    headers: vec![],
                    body: vec![],
                })
            },
            |duration| waits.push(duration.as_secs()),
        )
        .unwrap();
        assert_eq!((calls, waits, reply.status), (3, vec![1, 1], 503));
        let mut calls = 0;
        let mut waited = 0;
        let reply = read_backoff(
            || {
                calls += 1;
                Ok(Response {
                    status: if calls == 1 { 429 } else { 200 },
                    headers: vec![("retry-after".into(), "1".into())],
                    body: vec![],
                })
            },
            |duration| waited += duration.as_secs(),
        )
        .unwrap();
        assert_eq!((calls, waited, reply.status), (2, 1, 200));
        let mut calls = 0;
        assert!(read_backoff(
            || {
                calls += 1;
                Err(Error::Io("disconnected".into()))
            },
            |_| panic!("must not wait")
        )
        .is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn only_explicit_read_batches_allow_rate_limit_replay() {
        let mut request = JmapRequest {
            using: vec![],
            method_calls: vec![],
        };
        assert!(!read_batch(&request));
        request
            .method_calls
            .push(MethodCall("Email/get", json!({}), "a".into()));
        request
            .method_calls
            .push(MethodCall("Thread/get", json!({}), "b".into()));
        assert!(read_batch(&request));
        for name in [
            "Email/set",
            "EmailSubmission/set",
            "Email/import",
            "Something/get",
        ] {
            request
                .method_calls
                .push(MethodCall(name, json!({}), "c".into()));
            assert!(!read_batch(&request));
            request.method_calls.pop();
        }
    }

    /// The credential goes only where the request was configured to go:
    /// a redirect within that origin carries it, one to any other origin,
    /// TLS or not, is followed bare. The origin is scheme, host and
    /// effective port, case-insensitively, and it is the configured one
    /// that counts, not the last hop's.
    #[test]
    fn a_redirect_carries_the_credential_only_within_the_configured_origin() {
        let trusted = "https://mail.example/.well-known/jmap";
        for (location, target, carries) in [
            ("/jmap/session", "https://mail.example/jmap/session", true),
            ("session", "https://mail.example/.well-known/session", true),
            (
                "https://MAIL.example:443/s",
                "https://MAIL.example:443/s",
                true,
            ),
            (
                "https://api.example/session",
                "https://api.example/session",
                false,
            ),
            ("http://mail.example/x", "http://mail.example/x", false),
            ("//evil.example/x", "https://evil.example/x", false),
        ] {
            assert_eq!(
                JmapClient::redirect_target(trusted, trusted, location),
                (target.to_string(), carries),
                "{location}"
            );
        }
        assert_eq!(
            JmapClient::redirect_target(trusted, "https://api.example/s", "https://mail.example/t"),
            ("https://mail.example/t".to_string(), true)
        );
        let local = "http://127.0.0.1:8080/a";
        assert!(JmapClient::redirect_target(local, local, "http://127.0.0.1:8080/b").1);
        assert!(!JmapClient::redirect_target(local, local, "http://127.0.0.1:9090/b").1);
        assert!(
            JmapClient::redirect_target("http://Local.Test:80/a", local, "http://local.test/b").1
        );
        let err = JmapClient::left_the_origin(401, trusted, "https://api.example/s");
        assert!(
            err.to_string().contains("configure that URL directly"),
            "{err}"
        );
    }

    /// A base with no path is its own directory; the query is not part
    /// of the directory; a scheme-relative location keeps the scheme.
    #[test]
    fn a_relative_redirect_resolves_against_the_base_it_came_from() {
        let resolve = JmapClient::resolve_redirect;
        assert_eq!(
            resolve("https://mail.example", "session"),
            "https://mail.example/session"
        );
        assert_eq!(
            resolve("https://mail.example", "/s"),
            "https://mail.example/s"
        );
        assert_eq!(
            resolve("https://mail.example/", "s"),
            "https://mail.example/s"
        );
        assert_eq!(resolve("https://h/a/b?q=/x", "c"), "https://h/a/c");
        assert_eq!(resolve("https://h/a/b", "//other/c"), "https://other/c");
        assert_eq!(resolve("https://h/a/b", "http://o/c"), "http://o/c");
        assert_eq!(resolve("not a url", "c"), "c");
    }

    #[test]
    fn a_template_value_is_encoded_before_it_joins_the_url() {
        assert_eq!(uri_encode("a-b.c_d~E9"), "a-b.c_d~E9");
        assert_eq!(uri_encode("message/rfc822"), "message%2Frfc822");
        let hostile = uri_encode("x\nheader foo: bar?y#z");
        assert!(
            !hostile
                .bytes()
                .any(|b| b.is_ascii_control() || b"/?#: ".contains(&b)),
            "{hostile}"
        );
        assert_eq!(hostile, "x%0Aheader%20foo%3A%20bar%3Fy%23z");
    }
}
