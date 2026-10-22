use super::*;
use crate::{
    config::endpoint::HttpsUri,
    operator_files::{Inputs, Role},
    outbound_http::{Client, Request},
    store_fs::PrivateRoot,
};
use std::{cell::Cell, path::PathBuf};
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
        if username.is_empty()
            || username.len() > 998
            || username.contains(':')
            || username.chars().any(char::is_control)
        {
            return Err("migration-invalid-username");
        }
        let parent = Path::new(password_file)
            .parent()
            .and_then(Path::to_str)
            .ok_or("migration-secret-parent")?;
        let root = PrivateRoot::open(parent).map_err(|_| "migration-secret-parent")?;
        let mut inputs = Inputs::new();
        inputs.bind(&root).map_err(|_| "migration-secret-parent")?;
        let bytes = read_bounded(
            inputs
                .open(password_file, Role::Secret)
                .map_err(|_| "migration-secret-file")?,
            1024,
        )?;
        let password = std::str::from_utf8(&bytes).map_err(|_| "migration-invalid-password")?;
        let password = password
            .strip_suffix("\r\n")
            .or_else(|| password.strip_suffix('\n'))
            .unwrap_or(password);
        if password.is_empty() || password.chars().any(char::is_control) {
            return Err("migration-invalid-password");
        }
        let authorization = format!(
            "Basic {}",
            crate::acme::wire::base64(format!("{username}:{password}").as_bytes(), false)
                .map_err(|_| "migration-credential-capacity")?
        );
        let trust = match ca {
            Some(path) => TrustStore::from_pem(&read_bounded(
                inputs
                    .open(path, Role::Public)
                    .map_err(|_| "migration-trust-file")?,
                128 * 1024,
            )?),
            None => TrustStore::public_roots(),
        }
        .map_err(|_| "migration-trust")?;
        let mut source = Self {
            endpoint: endpoint.to_owned(),
            account: account.to_owned(),
            authorization,
            trust,
            executable: std::env::current_exe().map_err(|_| "migration-executable")?,
            api: String::new(),
            download: String::new(),
            deadline: Instant::now() + Duration::from_secs(24 * 3600),
            requests: Cell::new(0),
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
        sink: impl FnMut(&[u8]) -> std::result::Result<(), crate::outbound_http::Error>,
    ) -> Result<crate::outbound_http::Response> {
        self.same_origin(url)?;
        let count = self.requests.get();
        if count >= 200_000 {
            return Err("migration-request-capacity");
        }
        self.requests.set(count + 1);
        let response = Client {
            executable: &self.executable,
            origin: &self.endpoint,
            trust: &self.trust,
            deadline: self.deadline,
        }
        .exchange(
            Request {
                method,
                url,
                body,
                content_type: "application/json",
                accept: "application/json, message/rfc822, application/octet-stream",
                authorization: Some(&self.authorization),
                body_limit: maximum,
            },
            sink,
        )
        .map_err(|_| "migration-https-failed")?;
        if response.status != 200 {
            return Err("migration-http-status");
        }
        Ok(response)
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
            if let Some(id) = mailbox {
                if let Json::Obj(ref mut fields) = args {
                    fields.push(("filter".to_owned(), json!({"inMailbox":id})));
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
    if tuple.len() != 3
        || tuple.first().and_then(Json::as_str) != Some(method)
        || tuple.get(2).and_then(Json::as_str) != Some("export")
    {
        return Err("migration-jmap-method-failed");
    }
    let value = tuple.get(1).ok_or("migration-jmap-response")?;
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
