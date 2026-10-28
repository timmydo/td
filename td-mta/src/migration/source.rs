use super::*;
use crate::{
    config::endpoint::HttpsUri,
    outbound_http::{Client, Request},
};
use std::{cell::Cell, io::Write, path::PathBuf};
use td_crypto::TrustStore;

pub(super) struct Source {
    endpoint: String,
    account: String,
    authorization: String,
    trust: TrustStore,
    executable: PathBuf,
    api: String,
    download: String,
    deadline: Instant,
    requests: Cell<usize>,
    last_response: Cell<Option<Instant>>,
}
impl Source {
    pub fn open(
        endpoint: &str,
        account: &str,
        username: &str,
        password_file: &str,
        ca: Option<&str>,
    ) -> Result<Self> {
        HttpsUri::parse(endpoint).map_err(|_| "migration-invalid-url")?;
        let credentials = crate::outbound_credentials::Loaded::open(username, password_file, ca)
            .map_err(|_| "migration-source-credentials")?;
        let mut source = Self {
            endpoint: endpoint.to_owned(),
            account: account.to_owned(),
            authorization: credentials.authorization,
            trust: credentials.trust,
            executable: std::env::current_exe().map_err(|_| "migration-executable")?,
            api: String::new(),
            download: String::new(),
            deadline: Instant::now() + Duration::from_secs(24 * 3600),
            requests: Cell::new(0),
            last_response: Cell::new(None),
        };
        let session = source.json("GET", endpoint, &[])?;
        if field(&session, "accounts")?.get(account).is_none() {
            return Err("migration-account-not-found");
        }
        source.api = string(&session, "apiUrl")?.to_owned();
        source.download = string(&session, "downloadUrl")?.to_owned();
        source.same_origin(&source.api)?;
        source.download_url("probe")?;
        Ok(source)
    }
    fn same_origin(&self, url: &str) -> Result<()> {
        let origin = HttpsUri::parse(&self.endpoint).map_err(|_| "migration-invalid-url")?;
        let uri = HttpsUri::parse(url).map_err(|_| "migration-invalid-url")?;
        if !origin.same_origin(uri) {
            return Err("migration-cross-origin");
        }
        Ok(())
    }
    pub fn request(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
        maximum: usize,
        mut sink: impl FnMut(&[u8]) -> std::result::Result<(), crate::outbound_http::Error>,
    ) -> Result<crate::outbound_http::Response> {
        self.same_origin(url)?;
        // These are only authenticated read methods and raw GETs. A refused
        // status cannot have touched the caller's JSON/body sink.
        for attempt in 0..=BUSY_DELAYS.len() {
            let count = self.requests.get();
            if count >= 200_000 {
                return Err("migration-request-capacity");
            }
            let wait = request_wait(self.last_response.get(), Instant::now(), self.deadline)?;
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            if Instant::now() >= self.deadline {
                return Err("migration-network-deadline");
            }
            self.requests.set(count + 1);
            let response = Client {
                executable: &self.executable,
                origin: &self.endpoint,
                trust: &self.trust,
                deadline: self.deadline,
            }
            .exchange_success(
                Request {
                    method,
                    url,
                    body,
                    content_type: "application/json",
                    accept: "application/json, message/rfc822, application/octet-stream",
                    authorization: Some(&self.authorization),
                    body_limit: maximum,
                },
                &mut sink,
            );
            self.last_response.set(Some(Instant::now()));
            match response {
                Ok(response) => return Ok(response),
                Err(crate::outbound_http::Error::Status(status)) => {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "migration HTTP response status: {status}"
                    );
                    let delay = busy_delay(status, attempt, Instant::now(), self.deadline)?;
                    std::thread::sleep(delay);
                }
                Err(_) => return Err("migration-https-failed"),
            }
        }
        Err("migration-http-busy")
    }
    fn json(&self, method: &str, url: &str, body: &[u8]) -> Result<Json> {
        let mut bytes = Vec::new();
        self.request(method, url, body, 8 * 1024 * 1024, |part| {
            bytes.extend_from_slice(part);
            Ok(())
        })?;
        parse(&bytes)
    }
    fn call(&self, method: &str, arguments: Json) -> Result<Json> {
        let request=json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[[method,arguments,"export"]]}).to_string();
        let response = self.json("POST", &self.api, request.as_bytes())?;
        reply(&response, method, &self.account)
    }
    pub fn mailboxes(&self, state: &str) -> Result<Vec<Json>> {
        match self.query("Mailbox", None, MAILBOX_LIMIT) {
            Ok(ids) => {
                let mut output = Vec::new();
                let mut bytes = 0usize;
                for batch in ids.chunks(64) {
                    for mailbox in self.get("Mailbox", batch, state)? {
                        bytes = bytes
                            .checked_add(mailbox.to_string().len())
                            .filter(|n| *n <= INDEX_LIMIT / 2)
                            .ok_or("migration-capacity")?;
                        output.push(mailbox);
                    }
                }
                Ok(output)
            }
            Err("migration-jmap-unknown-method") => {
                let result = self.call(
                    "Mailbox/get",
                    json!({
                        "accountId": self.account, "ids": null,
                        "properties": ["id","name","parentId","role","sortOrder","isSubscribed"]
                    }),
                )?;
                all_mailboxes(&result, state)
            }
            Err(error) => Err(error),
        }
    }
    pub fn state(&self, kind: &str) -> Result<String> {
        let result = self.call(
            &format!("{kind}/get"),
            json!({"accountId":self.account,"ids":[]}),
        )?;
        if !array(&result, "list")?.is_empty() || !array(&result, "notFound")?.is_empty() {
            return Err("migration-jmap-response");
        }
        let state = string(&result, "state")?;
        identifier(state)?;
        Ok(state.to_owned())
    }
    pub fn query(&self, kind: &str, mailbox: Option<&str>, maximum: usize) -> Result<Vec<String>> {
        let mut output = Vec::new();
        let mut state = None;
        let mut total = None;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let mut args = json!({"accountId":self.account,"position":output.len(),"limit":64,"calculateTotal":true});
            if let Json::Obj(ref mut fields) = args {
                if let Some(id) = mailbox {
                    fields.push(("filter".to_owned(), json!({"inMailbox":id})));
                } else if kind == "Email" {
                    // Nonnegative sizes cover every Email, including empty bodies.
                    fields.push(("filter".to_owned(), json!({"minSize":0})));
                }
            }
            let result = self.call(&format!("{kind}/query"), args)?;
            let current = string(&result, "queryState")?;
            identifier(current)?;
            let count = integer(&result, "total")?;
            if count > maximum as u64 {
                return Err("migration-capacity");
            }
            if state.as_deref().is_some_and(|old| old != current)
                || total.is_some_and(|old| old != count)
            {
                return Err("migration-source-changed");
            }
            state = Some(current.to_owned());
            total = Some(count);
            if integer(&result, "position")? != output.len() as u64 {
                return Err("migration-query-position");
            }
            let ids = array(&result, "ids")?;
            if ids.len() > 64 || output.len() + ids.len() > count as usize {
                return Err("migration-query-count");
            }
            for id in ids {
                let id = id.as_str().ok_or("migration-invalid-id")?;
                identifier(id)?;
                if !seen.insert(id.to_owned()) {
                    return Err("migration-duplicate-id");
                }
                output.push(id.to_owned());
            }
            if output.len() == count as usize {
                return Ok(output);
            }
            if ids.is_empty() {
                return Err("migration-query-incomplete");
            }
        }
    }
    pub fn get(&self, kind: &str, ids: &[String], state: &str) -> Result<Vec<Json>> {
        let properties = if kind == "Email" {
            vec![
                "id",
                "blobId",
                "size",
                "receivedAt",
                "mailboxIds",
                "keywords",
            ]
        } else {
            vec![
                "id",
                "name",
                "parentId",
                "role",
                "sortOrder",
                "isSubscribed",
            ]
        };
        let result = self.call(
            &format!("{kind}/get"),
            json!({"accountId":self.account,"ids":ids,"properties":properties}),
        )?;
        if string(&result, "state")? != state {
            return Err("migration-source-changed");
        }
        if !array(&result, "notFound")?.is_empty() {
            return Err("migration-source-missing-object");
        }
        let list = array(&result, "list")?;
        let mut remaining: std::collections::BTreeSet<_> = ids.iter().map(String::as_str).collect();
        for object in list {
            if !remaining.remove(string(object, "id")?) {
                return Err("migration-unrequested-object");
            }
        }
        if !remaining.is_empty() {
            return Err("migration-source-missing-object");
        }
        Ok(list.to_vec())
    }
    pub fn absent(&self, kind: &str, ids: &[String], state: &str) -> Result<()> {
        if ids.len() > 64 {
            return Err("migration-capacity");
        }
        if !matches!(kind, "Email" | "Mailbox") {
            return Err("migration-arguments");
        }
        let result = self.call(
            &format!("{kind}/get"),
            json!({"accountId":self.account,"ids":ids,"properties":["id"]}),
        )?;
        absent_reply(&result, ids, state)
    }
    pub fn download_url(&self, blob: &str) -> Result<String> {
        identifier(blob)?;
        let mut url = self.download.clone();
        for (key, value) in [
            ("accountId", self.account.as_str()),
            ("blobId", blob),
            ("name", "message.eml"),
            ("type", "message/rfc822"),
        ] {
            url = url.replace(&format!("{{{key}}}"), &encode(value));
        }
        if url.contains(['{', '}']) {
            return Err("migration-download-template");
        }
        self.same_origin(&url)?;
        Ok(url)
    }
}
const BUSY_DELAYS: &[u64] = &[1, 2, 4, 8];
fn busy_delay(status: u16, attempt: usize, now: Instant, deadline: Instant) -> Result<Duration> {
    if !matches!(status, 429 | 503) {
        return Err("migration-http-status");
    }
    let seconds = *BUSY_DELAYS.get(attempt).ok_or("migration-http-busy")?;
    let delay = Duration::from_secs(seconds);
    if now.checked_add(delay).is_none_or(|next| next >= deadline) {
        return Err("migration-network-deadline");
    }
    Ok(delay)
}

// Leave room below td-mta's eight authenticated requests per peer-second.
fn request_wait(last: Option<Instant>, now: Instant, deadline: Instant) -> Result<Duration> {
    let next = match last {
        Some(last) => last
            .checked_add(Duration::from_millis(200))
            .ok_or("migration-network-deadline")?,
        None => now,
    };
    if now >= deadline || next >= deadline {
        return Err("migration-network-deadline");
    }
    Ok(next.saturating_duration_since(now))
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn reply(response: &Json, method: &str, account: &str) -> Result<Json> {
    let responses = array(response, "methodResponses")?;
    if responses.len() != 1 {
        return Err("migration-jmap-response");
    }
    let tuple = responses
        .first()
        .and_then(Json::as_array)
        .ok_or("migration-jmap-response")?;
    if tuple.len() != 3 || tuple.get(2).and_then(Json::as_str) != Some("export") {
        return Err("migration-jmap-method-failed");
    }
    let value = tuple.get(1).ok_or("migration-jmap-response")?;
    if tuple.first().and_then(Json::as_str) == Some("error")
        && value.get("type").and_then(Json::as_str) == Some("unknownMethod")
    {
        return Err("migration-jmap-unknown-method");
    }
    if tuple.first().and_then(Json::as_str) != Some(method) {
        return Err("migration-jmap-method-failed");
    }
    if string(value, "accountId")? != account {
        return Err("migration-account-mismatch");
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn busy_backoff_stops_before_the_original_deadline_and_attempt_ceiling() {
        let now = Instant::now();
        let end = now + Duration::from_secs(30);
        for status in [429, 503] {
            assert_eq!(busy_delay(status, 0, now, end), Ok(Duration::from_secs(1)));
            assert_eq!(busy_delay(status, 3, now, end), Ok(Duration::from_secs(8)));
            assert_eq!(busy_delay(status, 4, now, end), Err("migration-http-busy"));
            assert_eq!(
                busy_delay(status, 0, now, now + Duration::from_secs(1)),
                Err("migration-network-deadline")
            );
            assert_eq!(
                busy_delay(status, 0, end, now),
                Err("migration-network-deadline")
            );
        }
        for status in [301, 401, 403, 404, 500, 502, 504] {
            assert_eq!(
                busy_delay(status, 0, now, end),
                Err("migration-http-status")
            );
        }
    }
    #[test]
    fn response_is_bound_to_method_call_and_account() {
        let good = json!({"methodResponses":[["Email/get",{"accountId":"a","list":[]},"export"]]});
        assert!(reply(&good, "Email/get", "a").is_ok());
        assert!(reply(&good, "Mailbox/get", "a").is_err());
        assert!(reply(&good, "Email/get", "b").is_err());
        for bad in [
            json!({"methodResponses":[]}),
            json!({"methodResponses":[["error",{"type":"serverFail"},"export"]]}),
            json!({"methodResponses":[["Email/get",{"accountId":"a"},"other"]]}),
        ] {
            assert!(reply(&bad, "Email/get", "a").is_err());
        }
    }
    #[test]
    fn download_ids_are_percent_encoded_and_origin_cannot_change() {
        let mut source = Source {
            endpoint: "https://example.test/session".into(),
            account: "a/b".into(),
            authorization: String::new(),
            trust: TrustStore::public_roots().unwrap(),
            executable: PathBuf::new(),
            api: String::new(),
            download: "https://example.test/download/{accountId}/{blobId}/{name}?accept={type}"
                .into(),
            deadline: Instant::now(),
            requests: Cell::new(0),
            last_response: Cell::new(None),
        };
        assert_eq!(
            source.download_url("x?y#z").unwrap(),
            "https://example.test/download/a%2Fb/x%3Fy%23z/message.eml?accept=message%2Frfc822"
        );
        for bad in [
            "https://other.test/{blobId}",
            "http://example.test/{blobId}",
            "https://example.test/{unknown}",
        ] {
            source.download = bad.into();
            assert!(source.download_url("blob").is_err());
        }
    }
}

fn all_mailboxes(result: &Json, state: &str) -> Result<Vec<Json>> {
    if string(result, "state")? != state {
        return Err("migration-source-changed");
    }
    if !array(result, "notFound")?.is_empty() {
        return Err("migration-source-missing-object");
    }
    let list = array(result, "list")?;
    if list.len() > MAILBOX_LIMIT {
        return Err("migration-capacity");
    }
    let mut seen = std::collections::BTreeSet::new();
    for mailbox in list {
        let id = string(mailbox, "id")?;
        identifier(id)?;
        if !seen.insert(id) {
            return Err("migration-duplicate-id");
        }
    }
    Ok(list.to_vec())
}

#[cfg(test)]
mod fallback_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn request_pacing_stays_below_peer_limit_without_renewing_deadline() {
        let _serial = crate::generations::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(60);
        let peer = std::net::IpAddr::from([127, 0, 0, 1]);
        let mut unpaced = crate::http_auth::Attempts::default();
        assert!((0..8).all(|_| unpaced.admit(peer, 0)));
        assert!(!unpaced.admit(peer, 0));
        for offset in [0, 199, 999] {
            let mut attempts = crate::http_auth::Attempts::default();
            let mut now = start + Duration::from_millis(offset);
            let mut last = None;
            for _ in 0..20 {
                now += request_wait(last, now, deadline).unwrap();
                assert!(attempts.admit(peer, now.duration_since(start).as_millis() as u64));
                last = Some(now);
            }
        }
        assert!(request_wait(Some(start), start, start + Duration::from_millis(199)).is_err());
        assert!(request_wait(None, deadline, deadline).is_err());
        assert_eq!(
            request_wait(Some(start), start + Duration::from_secs(1), deadline).unwrap(),
            Duration::ZERO
        );
    }
    #[test]
    fn only_correlated_unknown_method_is_distinguished() {
        let unknown = json!({"methodResponses":[["error",{"type":"unknownMethod"},"export"]]});
        assert_eq!(
            reply(&unknown, "Mailbox/query", "a").unwrap_err(),
            "migration-jmap-unknown-method"
        );
        for bad in [
            json!({"methodResponses":[["error",{"type":"unknownMethod"},"other"]]}),
            json!({"methodResponses":[["error",{"type":"serverFail"},"export"]]}),
            json!({"methodResponses":[["error",{"type":"unknownMethod"},"export","extra"]]}),
            json!({"methodResponses":[["error",{"type":"unknownMethod"},"export"],["error",{"type":"unknownMethod"},"export"]]}),
        ] {
            assert_ne!(
                reply(&bad, "Mailbox/query", "a").unwrap_err(),
                "migration-jmap-unknown-method"
            );
        }
    }
    #[test]
    fn get_all_fallback_checks_state_completeness_uniqueness_and_bound() {
        let value = json!({"state":"s","notFound":[],"list":[{"id":"one"},{"id":"two"}]});
        assert_eq!(all_mailboxes(&value, "s").unwrap().len(), 2);
        assert_eq!(
            all_mailboxes(&value, "changed").unwrap_err(),
            "migration-source-changed"
        );
        for bad in [
            json!({"state":"s","notFound":["missing"],"list":[]}),
            json!({"state":"s","notFound":[],"list":[{"id":"one"},{"id":"one"}]}),
            json!({"state":"s","notFound":[],"list":[{"id":""}]}),
            json!({"state":"s","notFound":[],"list":[{}]}),
            json!({"state":"s","notFound":[],"list":vec![json!({"id":"x"});MAILBOX_LIMIT+1]}),
        ] {
            assert!(all_mailboxes(&bad, "s").is_err());
        }
    }
}

fn absent_reply(result: &Json, ids: &[String], state: &str) -> Result<()> {
    if string(result, "state")? != state {
        return Err("migration-source-changed");
    }
    if !array(result, "list")?.is_empty() {
        return Err("migration-source-object-present");
    }
    let mut remaining: std::collections::BTreeSet<_> = ids.iter().map(String::as_str).collect();
    if remaining.len() != ids.len() {
        return Err("migration-duplicate-id");
    }
    for id in array(result, "notFound")? {
        if !remaining.remove(id.as_str().ok_or("migration-invalid-id")?) {
            return Err("migration-unrequested-object");
        }
    }
    if !remaining.is_empty() {
        return Err("migration-source-absence-unproven");
    }
    Ok(())
}

#[cfg(test)]
mod absence_tests {
    use super::*;
    #[test]
    fn absence_requires_exact_not_found_and_unchanged_state() {
        let ids = vec!["one".to_owned(), "two".to_owned()];
        assert!(absent_reply(
            &json!({"state":"e","list":[],"notFound":["two","one"]}),
            &ids,
            "e"
        )
        .is_ok());
        for response in [
            json!({"state":"other","list":[],"notFound":["one","two"]}),
            json!({"state":"e","list":[{"id":"one"}],"notFound":["two"]}),
            json!({"state":"e","list":[],"notFound":["one"]}),
            json!({"state":"e","list":[],"notFound":["one","one"]}),
            json!({"state":"e","list":[],"notFound":["one","other"]}),
            json!({"state":"e","list":[],"notFound":["one","two","extra"]}),
        ] {
            assert!(absent_reply(&response, &ids, "e").is_err());
        }
    }
}
