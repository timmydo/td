//! Read-only source transport. No local writes or durable checkpoint authority.
use super::{Binding, Bounds, Inventory, Metadata, SourceLimits};
use crate::{
    bounded::TextBuffer,
    config::endpoint::Origin,
    ids::{AccountId, BlobId, EmailId, StoreEpoch},
    outbound_http::{self, Client, Request, ReusableClient},
    postgres_read::ReplicaInventoryCursor,
    wire,
};
use std::time::Instant;
use td_crypto::{BulkSha256, Digest};
use td_json::{scan, Json};

const CALL: &str = "replica";
const INVENTORY: &str = "TdReplica/inventory";
const METADATA: &str = "TdReplica/metadata";
const MAX_JSON: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 262144;
const JSON_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    Transport(outbound_http::Error),
    Source(super::Error),
    Invalid,
    Capacity,
    Deadline,
    Integrity,
    Crypto,
    Unbound,
}
impl From<outbound_http::Error> for ReadError {
    fn from(value: outbound_http::Error) -> Self {
        Self::Transport(value)
    }
}
impl From<super::Error> for ReadError {
    fn from(value: super::Error) -> Self {
        Self::Source(value)
    }
}

/// Caller accounts for retained pages plus one whole provisional body per reader.
#[derive(Clone, Copy)]
pub struct ReadLimits {
    pub json_bytes: usize,
    pub json_nodes: usize,
    pub email: Bounds,
}
impl ReadLimits {
    fn validate(self) -> Result<(), ReadError> {
        if self.json_bytes == 0
            || self.json_bytes > MAX_JSON
            || self.json_nodes == 0
            || self.json_nodes > MAX_NODES
            || self.email.body_bytes > crate::postgres_body::MAX_BODY_BYTES
            || self.email.values_per_email == 0
            || self.email.values_per_email > super::MAX_VALUES_PER_EMAIL
        {
            return Err(ReadError::Capacity);
        }
        Ok(())
    }
}

pub struct Reader<'a> {
    http: ReusableClient<'a>,
    origin: &'a str,
    authorization: &'a str,
    deadline: Instant,
    session: Json,
    account: AccountId,
    epoch: Option<StoreEpoch>,
    limits: ReadLimits,
}
impl<'a> Reader<'a> {
    /// A resumed reader requires the persisted binding. None permits only the
    /// sole discovered account and learns its epoch from a valid inventory page.
    pub fn connect(
        client: Client<'a>,
        authorization: &'a str,
        expected: Option<Binding>,
        limits: ReadLimits,
    ) -> Result<Self, ReadError> {
        limits.validate()?;
        Origin::parse(client.origin).map_err(|_| ReadError::Invalid)?;
        let origin = client.origin;
        let deadline = client.deadline;
        let mut url_bytes = [0; 4096];
        let mut url = TextBuffer::new(&mut url_bytes);
        url.append(origin.trim_end_matches('/'))
            .map_err(|_| ReadError::Capacity)?;
        url.append("/.well-known/jmap")
            .map_err(|_| ReadError::Capacity)?;
        let mut http = ReusableClient::new(client)?;
        let session = fetch_json(
            &mut http,
            authorization,
            deadline,
            url.as_str().map_err(|_| ReadError::Invalid)?,
            &[],
            limits.json_bytes.min(32768),
            limits.json_nodes.min(8192),
        )?;
        let discovery = super::discover(&session, origin, expected.map(|v| v.account))?;
        let account = discovery.account();
        check_time(deadline)?;
        Ok(Self {
            http,
            origin,
            authorization,
            deadline,
            session,
            account,
            epoch: expected.map(|v| v.epoch),
            limits,
        })
    }
    pub fn account(&self) -> AccountId {
        self.account
    }
    pub fn binding(&self) -> Option<Binding> {
        self.epoch.map(|epoch| Binding {
            account: self.account,
            epoch,
        })
    }
    pub fn source_limits(&self) -> Result<SourceLimits, ReadError> {
        Ok(super::discover(&self.session, self.origin, Some(self.account))?.limits())
    }
    pub fn inventory(
        &mut self,
        previous: Option<ReplicaInventoryCursor>,
    ) -> Result<Inventory, ReadError> {
        if let Some(cursor) = previous {
            let bound = self.binding().ok_or(ReadError::Unbound)?;
            if cursor.base.account != bound.account || cursor.base.epoch != bound.epoch {
                return Err(super::Error::SourceChanged.into());
            }
        }
        let limit = self.source_limits()?.inventory_ids;
        let mut bytes = [0; 4096];
        let mut out = TextBuffer::new(&mut bytes);
        request_start(&mut out, INVENTORY, self.account)?;
        out.format(format_args!(",\"limit\":{limit},\"cursor\":"))
            .map_err(|_| ReadError::Capacity)?;
        if let Some(cursor) = previous {
            let mut encoded = [0; ReplicaInventoryCursor::WIRE_BYTES];
            let text = cursor
                .encode(&mut encoded)
                .map_err(|_| ReadError::Invalid)?;
            out.format(format_args!("\"{text}\""))
                .map_err(|_| ReadError::Capacity)?;
        } else {
            out.append("null").map_err(|_| ReadError::Capacity)?;
        }
        request_end(&mut out)?;
        let response = self.api(out.as_str().map_err(|_| ReadError::Invalid)?.as_bytes())?;
        let value = super::reply(&response, INVENTORY, CALL)?;
        let epoch =
            StoreEpoch::parse(super::text(value, "sourceId")?).map_err(|_| ReadError::Invalid)?;
        let binding = self.binding().unwrap_or(Binding {
            account: self.account,
            epoch,
        });
        let page = super::inventory(value, binding, previous, limit)?;
        check_time(self.deadline)?;
        self.epoch = Some(binding.epoch);
        Ok(page)
    }
    pub fn metadata(&mut self, requested: &[EmailId]) -> Result<MetadataPage, ReadError> {
        let binding = self.binding().ok_or(ReadError::Unbound)?;
        if requested.len() > self.source_limits()?.metadata_ids {
            return Err(ReadError::Capacity);
        }
        let mut ids = Vec::new();
        ids.try_reserve_exact(requested.len())
            .map_err(|_| ReadError::Capacity)?;
        let mut bytes = [0; 4096];
        let mut out = TextBuffer::new(&mut bytes);
        request_start(&mut out, METADATA, self.account)?;
        out.append(",\"ids\":[").map_err(|_| ReadError::Capacity)?;
        for id in requested {
            if ids.contains(id) {
                return Err(ReadError::Invalid);
            }
            if !ids.is_empty() {
                out.append(",").map_err(|_| ReadError::Capacity)?;
            }
            let mut encoded = [0; wire::IdKind::WIRE_BYTES];
            let text = wire::encode_id(*id, &mut encoded).map_err(|_| ReadError::Invalid)?;
            out.format(format_args!("\"{text}\""))
                .map_err(|_| ReadError::Capacity)?;
            ids.push(*id);
        }
        out.append("]").map_err(|_| ReadError::Capacity)?;
        request_end(&mut out)?;
        let response = self.api(out.as_str().map_err(|_| ReadError::Invalid)?.as_bytes())?;
        let page = MetadataPage::decode(self.origin, binding, ids, self.limits.email, response)?;
        check_time(self.deadline)?;
        Ok(page)
    }
    pub fn body(&mut self, page: &MetadataPage, id: EmailId) -> Result<VerifiedBody, ReadError> {
        let binding = self.binding().ok_or(ReadError::Unbound)?;
        if binding.account != page.binding.account
            || binding.epoch != page.binding.epoch
            || !Origin::parse(self.origin)
                .map_err(|_| ReadError::Invalid)?
                .same_origin(Origin::parse(&page.origin).map_err(|_| ReadError::Invalid)?)
        {
            return Err(super::Error::SourceChanged.into());
        }
        let view = page.view()?;
        let email = view
            .emails
            .iter()
            .find(|email| email.id == id)
            .ok_or(ReadError::Invalid)?;
        if email.size > self.limits.email.body_bytes {
            return Err(ReadError::Capacity);
        }
        check_time(self.deadline)?;
        let url = super::discover(&self.session, self.origin, Some(self.account))?
            .download_url(email.blob)?;
        let mut staging = Staging::new(email.blob, email.size, email.digest)?;
        let mut failure = None;
        let result = self.http.exchange_success(
            Request {
                method: "GET",
                url: &url,
                body: &[],
                content_type: "message/rfc822",
                accept: "message/rfc822",
                authorization: Some(self.authorization),
                body_limit: staging.expected,
            },
            |part| {
                staging.append(part).map_err(|error| {
                    failure = Some(error);
                    outbound_http::Error::Sink
                })
            },
        );
        if let Some(error) = failure {
            return Err(error);
        }
        let response = result?;
        check_time(self.deadline)?;
        let body = staging.finish(response.bytes)?;
        check_time(self.deadline)?;
        Ok(body)
    }
    pub fn mailboxes(&mut self) -> Result<MailboxPage, ReadError> {
        let binding = self.binding().ok_or(ReadError::Unbound)?;
        let mut bytes = [0; 4096];
        let mut out = TextBuffer::new(&mut bytes);
        request_start(&mut out, "Mailbox/get", self.account)?;
        out.append(",\"ids\":null,\"properties\":[\"id\",\"name\",\"parentId\",\"role\",\"sortOrder\",\"isSubscribed\"]")
            .map_err(|_| ReadError::Capacity)?;
        request_end(&mut out)?;
        let response = self.api(out.as_str().map_err(|_| ReadError::Invalid)?.as_bytes())?;
        let page = MailboxPage::decode(self.origin, binding, response)?;
        check_time(self.deadline)?;
        Ok(page)
    }
    pub fn email_changes(
        &mut self,
        since: crate::sync::DataState,
    ) -> Result<super::Changes<EmailId>, ReadError> {
        let binding = self.binding().ok_or(ReadError::Unbound)?;
        let response = self.changes("Email/changes", since, crate::sync::DataType::Email)?;
        let value = super::reply(&response, "Email/changes", CALL)?;
        let page = super::email_changes(value, binding, since, crate::jmap_changes::MAX_CHANGES)?;
        check_time(self.deadline)?;
        Ok(page)
    }
    pub fn mailbox_changes(
        &mut self,
        since: crate::sync::DataState,
    ) -> Result<super::Changes<crate::ids::MailboxId>, ReadError> {
        let binding = self.binding().ok_or(ReadError::Unbound)?;
        let response = self.changes("Mailbox/changes", since, crate::sync::DataType::Mailbox)?;
        let value = super::reply(&response, "Mailbox/changes", CALL)?;
        let page = super::mailbox_changes(value, binding, since, crate::jmap_changes::MAX_CHANGES)?;
        check_time(self.deadline)?;
        Ok(page)
    }
    fn changes(
        &mut self,
        method: &str,
        since: crate::sync::DataState,
        kind: crate::sync::DataType,
    ) -> Result<Json, ReadError> {
        let binding = self.binding().ok_or(ReadError::Unbound)?;
        if since.account != binding.account || since.epoch != binding.epoch {
            return Err(super::Error::SourceChanged.into());
        }
        if since.kind != kind {
            return Err(ReadError::Invalid);
        }
        let mut bytes = [0; 4096];
        let mut out = TextBuffer::new(&mut bytes);
        request_start(&mut out, method, self.account)?;
        let mut encoded = [0; crate::sync::DataState::WIRE_BYTES];
        let state = since.encode(&mut encoded).map_err(|_| ReadError::Invalid)?;
        out.format(format_args!(
            ",\"sinceState\":\"{state}\",\"maxChanges\":{}",
            crate::jmap_changes::MAX_CHANGES
        ))
        .map_err(|_| ReadError::Capacity)?;
        request_end(&mut out)?;
        self.api(out.as_str().map_err(|_| ReadError::Invalid)?.as_bytes())
    }
    fn api(&mut self, body: &[u8]) -> Result<Json, ReadError> {
        let discovery = super::discover(&self.session, self.origin, Some(self.account))?;
        if body.len() > discovery.limits().request_bytes {
            return Err(ReadError::Capacity);
        }
        fetch_json(
            &mut self.http,
            self.authorization,
            self.deadline,
            discovery.api_url(),
            body,
            self.limits.json_bytes,
            self.limits.json_nodes,
        )
    }
}

/// Owns immutable bounded response storage; keywords in view() borrow this owner.
pub struct MetadataPage {
    origin: String,
    response: Json,
    binding: Binding,
    requested: Vec<EmailId>,
    bounds: Bounds,
}
impl MetadataPage {
    /// Caller supplies bounded response storage and authenticated source binding.
    /// This validates data; it does not authenticate the claimed origin.
    pub fn decode(
        origin: &str,
        binding: Binding,
        requested: Vec<EmailId>,
        bounds: Bounds,
        response: Json,
    ) -> Result<Self, ReadError> {
        Origin::parse(origin).map_err(|_| ReadError::Invalid)?;
        if requested.len() > crate::postgres_body::MAX_METADATA_IDS {
            return Err(ReadError::Capacity);
        }
        let mut owned = String::new();
        owned
            .try_reserve_exact(origin.len())
            .map_err(|_| ReadError::Capacity)?;
        owned.push_str(origin);
        let page = Self {
            origin: owned,
            response,
            binding,
            requested,
            bounds,
        };
        page.view()?;
        Ok(page)
    }
    pub fn source_origin(&self) -> &str {
        &self.origin
    }
    pub fn view(&self) -> Result<Metadata<'_>, ReadError> {
        let value = super::reply(&self.response, METADATA, CALL)?;
        Ok(super::metadata(
            value,
            self.binding,
            &self.requested,
            self.bounds,
        )?)
    }
}

/// A complete mailbox observation; it grants no Email deletion authority.
pub struct MailboxPage {
    origin: String,
    response: Json,
    binding: Binding,
}
impl MailboxPage {
    /// Decode caller-owned, already bounded response storage. The origin is a
    /// caller binding, not evidence that this function authenticated a peer.
    pub fn decode(origin: &str, binding: Binding, response: Json) -> Result<Self, ReadError> {
        Origin::parse(origin).map_err(|_| ReadError::Invalid)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(origin.len())
            .map_err(|_| ReadError::Capacity)?;
        owned.push_str(origin);
        let page = Self {
            origin: owned,
            response,
            binding,
        };
        page.view()?;
        Ok(page)
    }
    pub fn source_origin(&self) -> &str {
        &self.origin
    }
    pub fn view(&self) -> Result<super::Mailboxes<'_>, ReadError> {
        let value = super::reply(&self.response, "Mailbox/get", CALL)?;
        Ok(super::mailboxes(value, self.binding)?)
    }
}

/// Complete source-verified bytes. This conveys no database publication authority.
pub struct VerifiedBody {
    blob: BlobId,
    digest: [u8; 32],
    bytes: Vec<u8>,
}
impl VerifiedBody {
    pub fn blob(&self) -> BlobId {
        self.blob
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
struct Staging {
    blob: BlobId,
    expected: usize,
    digest: [u8; 32],
    hash: BulkSha256,
    bytes: Vec<u8>,
}
impl Staging {
    fn new(blob: BlobId, size: u64, digest: [u8; 32]) -> Result<Self, ReadError> {
        let expected = usize::try_from(size).map_err(|_| ReadError::Capacity)?;
        if expected > outbound_http::MAX_BODY {
            return Err(ReadError::Capacity);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(expected)
            .map_err(|_| ReadError::Capacity)?;
        Ok(Self {
            blob,
            expected,
            digest,
            hash: BulkSha256::try_new().map_err(|_| ReadError::Crypto)?,
            bytes,
        })
    }
    fn append(&mut self, part: &[u8]) -> Result<(), ReadError> {
        if part.len() > self.expected.saturating_sub(self.bytes.len()) {
            return Err(ReadError::Integrity);
        }
        self.hash.update(part).map_err(|_| ReadError::Crypto)?;
        self.bytes.extend_from_slice(part);
        Ok(())
    }
    fn finish(self, received: usize) -> Result<VerifiedBody, ReadError> {
        if self.bytes.len() != self.expected
            || received != self.expected
            || self.hash.finish().map_err(|_| ReadError::Crypto)? != self.digest
        {
            return Err(ReadError::Integrity);
        }
        Ok(VerifiedBody {
            blob: self.blob,
            digest: self.digest,
            bytes: self.bytes,
        })
    }
}
fn request_start(
    out: &mut TextBuffer<'_>,
    method: &str,
    account: AccountId,
) -> Result<(), ReadError> {
    let mut encoded = [0; wire::IdKind::WIRE_BYTES];
    let account = wire::encode_id(account, &mut encoded).map_err(|_| ReadError::Invalid)?;
    out.format(format_args!("{{\"using\":[\"{}\",\"{}\",\"{}\"],\"methodCalls\":[[\"{method}\",{{\"accountId\":\"{account}\"",
        crate::jmap_request::CORE, crate::jmap_request::MAIL, crate::jmap_request::REPLICATION))
        .map_err(|_| ReadError::Capacity)
}
fn request_end(out: &mut TextBuffer<'_>) -> Result<(), ReadError> {
    out.append("},\"replica\"]]}")
        .map_err(|_| ReadError::Capacity)
}
fn check_time(deadline: Instant) -> Result<(), ReadError> {
    if Instant::now() >= deadline {
        Err(ReadError::Deadline)
    } else {
        Ok(())
    }
}
fn fetch_json(
    http: &mut ReusableClient<'_>,
    authorization: &str,
    deadline: Instant,
    url: &str,
    body: &[u8],
    maximum: usize,
    nodes: usize,
) -> Result<Json, ReadError> {
    let mut bytes = Vec::new();
    let mut capacity_failure = false;
    let response = http.exchange_success(
        Request {
            method: if body.is_empty() { "GET" } else { "POST" },
            url,
            body,
            content_type: "application/json",
            accept: "application/json",
            authorization: Some(authorization),
            body_limit: maximum,
        },
        |part| {
            if part.len() > maximum.saturating_sub(bytes.len())
                || bytes.try_reserve(part.len()).is_err()
            {
                capacity_failure = true;
                return Err(outbound_http::Error::Sink);
            }
            bytes.extend_from_slice(part);
            Ok(())
        },
    );
    if capacity_failure {
        return Err(ReadError::Capacity);
    }
    let response = response?;
    if !response
        .content_type
        .as_deref()
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(ReadError::Invalid);
    }
    parse_json(&bytes, maximum, nodes, deadline)
}
fn parse_json(
    bytes: &[u8],
    maximum: usize,
    nodes: usize,
    deadline: Instant,
) -> Result<Json, ReadError> {
    check_time(deadline)?;
    if bytes.len() > maximum
        || maximum == 0
        || maximum > MAX_JSON
        || nodes == 0
        || nodes > MAX_NODES
    {
        return Err(ReadError::Capacity);
    }
    let mut scanner =
        scan::Scanner::new(maximum as u64, JSON_DEPTH).map_err(|_| ReadError::Invalid)?;
    let mut rest = bytes;
    let mut count = 0usize;
    let mut turn = 0usize;
    loop {
        let progress = scanner.poll(rest, true).map_err(|_| ReadError::Invalid)?;
        rest = rest.get(progress.consumed..).ok_or(ReadError::Invalid)?;
        if matches!(
            progress.event,
            Some(scan::Event::Start { .. } | scan::Event::KeyStart)
        ) {
            count = count.checked_add(1).ok_or(ReadError::Capacity)?;
            if count > nodes {
                return Err(ReadError::Capacity);
            }
        }
        turn += progress.consumed;
        if turn >= scan::TURN_BYTES {
            check_time(deadline)?;
            turn = 0;
        }
        if progress.complete {
            break;
        }
    }
    if !rest.is_empty() {
        return Err(ReadError::Invalid);
    }
    check_time(deadline)?;
    let value = td_json::parse_slice(bytes).map_err(|_| ReadError::Invalid)?;
    check_time(deadline)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::time::Duration;
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }
    fn hash(bytes: &[u8]) -> [u8; 32] {
        let mut hash = BulkSha256::try_new().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }
    #[test]
    fn json_allocations_are_bounded_before_tree_construction() {
        let bytes = br#"{"values":[1,true,null,"hello"]}"#;
        assert_eq!(
            parse_json(bytes, bytes.len(), 7, deadline()).unwrap(),
            td_json::parse_slice(bytes).unwrap()
        );
        assert_eq!(
            parse_json(bytes, bytes.len(), 6, deadline()).err(),
            Some(ReadError::Capacity)
        );
        assert_eq!(
            parse_json(bytes, bytes.len() - 1, 7, deadline()).err(),
            Some(ReadError::Capacity)
        );
        assert_eq!(
            parse_json(bytes, MAX_JSON + 1, 7, deadline()).err(),
            Some(ReadError::Capacity)
        );
        assert_eq!(
            parse_json(bytes, bytes.len(), MAX_NODES + 1, deadline()).err(),
            Some(ReadError::Capacity)
        );
        assert_eq!(
            parse_json(bytes, bytes.len(), 7, Instant::now()).err(),
            Some(ReadError::Deadline)
        );
        for invalid in [
            b"".as_slice(),
            b"{",
            b"{}{}",
            b"[1,]",
            b"\"\\uD800\"",
            b"01",
        ] {
            assert_eq!(
                parse_json(invalid, 1024, 64, deadline()).err(),
                Some(ReadError::Invalid)
            );
        }
        let deep = format!(
            "{}0{}",
            "[".repeat(JSON_DEPTH + 1),
            "]".repeat(JSON_DEPTH + 1)
        );
        assert_eq!(
            parse_json(deep.as_bytes(), 1024, 64, deadline()).err(),
            Some(ReadError::Invalid)
        );
        let wide = format!("[{}0]", "null,".repeat(20000));
        assert_eq!(
            parse_json(wide.as_bytes(), wide.len(), 100, deadline()).err(),
            Some(ReadError::Capacity)
        );
        assert!(parse_json(wide.as_bytes(), wide.len(), 20002, deadline()).is_ok());
    }
    #[test]
    fn body_exposure_requires_exact_length_and_complete_digest() {
        let blob = BlobId::from_bytes([3; 16]);
        let bytes = b"Subject: full body\r\n\r\nattachments included\r\n";
        let digest = hash(bytes);
        for chunk in [1, 7, bytes.len()] {
            let mut pending = Staging::new(blob, bytes.len() as u64, digest).unwrap();
            for part in bytes.chunks(chunk) {
                pending.append(part).unwrap();
            }
            let body = pending.finish(bytes.len()).unwrap();
            assert_eq!(body.blob(), blob);
            assert_eq!(body.digest(), digest);
            assert_eq!(body.bytes(), bytes);
        }
        let mut short = Staging::new(blob, bytes.len() as u64, digest).unwrap();
        short.append(b"Subject:").unwrap();
        assert_eq!(short.finish(8).err(), Some(ReadError::Integrity));
        let mut wrong = Staging::new(blob, bytes.len() as u64, [0; 32]).unwrap();
        wrong.append(bytes).unwrap();
        assert_eq!(wrong.finish(bytes.len()).err(), Some(ReadError::Integrity));
        let mut count = Staging::new(blob, bytes.len() as u64, digest).unwrap();
        count.append(bytes).unwrap();
        assert_eq!(
            count.finish(bytes.len() + 1).err(),
            Some(ReadError::Integrity)
        );
        let mut overflow = Staging::new(blob, 1, hash(b"x")).unwrap();
        assert_eq!(overflow.append(b"xx"), Err(ReadError::Integrity));
        assert!(overflow.bytes.is_empty());
        assert_eq!(
            Staging::new(blob, outbound_http::MAX_BODY as u64 + 1, digest).err(),
            Some(ReadError::Capacity)
        );
        let empty = Staging::new(blob, 0, hash(b"")).unwrap().finish(0).unwrap();
        assert!(empty.bytes().is_empty());
    }
    #[test]
    fn request_envelope_carries_required_capabilities_and_single_call() {
        let mut bytes = [0; 4096];
        let mut out = TextBuffer::new(&mut bytes);
        request_start(&mut out, INVENTORY, AccountId::from_bytes([1; 16])).unwrap();
        out.append(",\"cursor\":null,\"limit\":256").unwrap();
        request_end(&mut out).unwrap();
        let request = td_json::parse(out.as_str().unwrap()).unwrap();
        assert_eq!(request.get("using").unwrap().as_array().unwrap().len(), 3);
        let calls = request.get("methodCalls").unwrap().as_array().unwrap();
        assert_eq!(calls.len(), 1);
        let call = calls.first().unwrap().as_array().unwrap();
        assert_eq!(call.first().unwrap().as_str(), Some(INVENTORY));
        assert_eq!(call.get(2).unwrap().as_str(), Some(CALL));
        assert_eq!(
            call.get(1).unwrap().get("accountId").unwrap().as_str(),
            Some("a01010101010101010101010101010101")
        );
        let mut tiny = [0; 8];
        assert_eq!(
            request_start(
                &mut TextBuffer::new(&mut tiny),
                INVENTORY,
                AccountId::from_bytes([1; 16])
            ),
            Err(ReadError::Capacity)
        );
    }
    #[test]
    fn invalid_limits_and_origin_refuse_before_resolving() {
        let trust = td_crypto::TrustStore::public_roots().unwrap();
        let limits = ReadLimits {
            json_bytes: 32768,
            json_nodes: 8192,
            email: Bounds {
                body_bytes: 1024,
                values_per_email: 256,
            },
        };
        for origin in [
            "http://localhost",
            "https://localhost/path",
            "https://localhost/#bad",
        ] {
            let client = Client {
                executable: std::path::Path::new("/missing-resolver"),
                origin,
                trust: &trust,
                deadline: deadline(),
            };
            assert_eq!(
                Reader::connect(client, "Basic fixture", None, limits).err(),
                Some(ReadError::Invalid)
            );
        }
        let mut bad = limits;
        bad.email.values_per_email = 0;
        assert_eq!(bad.validate(), Err(ReadError::Capacity));
        bad = limits;
        bad.email.body_bytes = crate::postgres_body::MAX_BODY_BYTES + 1;
        assert_eq!(bad.validate(), Err(ReadError::Capacity));
    }
}
