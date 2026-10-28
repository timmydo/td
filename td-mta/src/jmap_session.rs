//! Prepared Mail Session metadata for the supported td-mail profile.
use crate::{
    bounded::TextBuffer,
    config::{
        endpoint::Origin, identities::AccountView, identity::MAX_NAME_BYTES,
        values::MAX_DOMAIN_BYTES,
    },
    limits::ResourcePlan,
    ports::Error,
    wire,
};
use td_crypto::Digest;
use td_json::string::{Frame, Scalar, Source, Status};

pub const SESSION_PATH: &str = "/.well-known/jmap";
pub const API_PATH: &str = "/jmap/api";
pub const UPLOAD_TEMPLATE: &str = "/jmap/upload/{accountId}";
pub const DOWNLOAD_TEMPLATE: &str = "/jmap/download/{accountId}/{blobId}/{name}?type={type}";
pub const EVENT_TEMPLATE: &str = "/jmap/events?types={types}&closeafter={closeafter}&ping={ping}";
pub(crate) const MAX_BYTES: usize = 32 * 1024;

/// Per-endpoint limits enforced by the runtime within its shared pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Concurrency {
    pub uploads: usize,
    pub requests: usize,
}

/// Immutable metadata for one authenticated account and one admission plan.
/// The caller must enforce both concurrency counts and complete the Core
/// and td-mail read/flag/move/folder contracts before exposing this document.
/// It conveys no authentication.
pub struct Session {
    origin: String,
    submission: bool,
    read_only: bool,
    concurrency: Concurrency,
    bytes: Vec<u8>,
    state: [u8; 64],
}
impl std::fmt::Debug for Session {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Session(<redacted>)")
    }
}
impl Session {
    pub fn mail_access(
        origin: Origin<'_>,
        account: AccountView<'_>,
        resources: &ResourcePlan,
        concurrency: Concurrency,
    ) -> Result<Self, Error> {
        Self::prepare(origin, account, resources, concurrency, false, false)
    }
    /// Prepared PostgreSQL sending profile; caller must implement the td-mail
    /// send and reconciliation flows before exposing this Session.
    pub fn mail_submission(
        origin: Origin<'_>,
        account: AccountView<'_>,
        resources: &ResourcePlan,
        concurrency: Concurrency,
    ) -> Result<Self, Error> {
        Self::prepare(origin, account, resources, concurrency, true, false)
    }
    /// Read-only mail metadata. Callers must enforce this policy on every
    /// request and separately qualify the database for replica reads.
    pub fn mail_read_only(
        origin: Origin<'_>,
        account: AccountView<'_>,
        resources: &ResourcePlan,
        concurrency: Concurrency,
    ) -> Result<Self, Error> {
        Self::prepare(origin, account, resources, concurrency, false, true)
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    /// Closed allowlist: future methods cannot silently grant replica writes.
    pub fn permits_method(&self, method: &[u8]) -> bool {
        !self.read_only
            || matches!(
                method,
                b"Core/echo"
                    | b"Mailbox/get"
                    | b"Mailbox/query"
                    | b"Mailbox/queryChanges"
                    | b"Mailbox/changes"
                    | b"Email/get"
                    | b"Email/query"
                    | b"Email/changes"
                    | b"Thread/get"
                    | b"Thread/changes"
                    | b"TdReplica/inventory"
                    | b"TdReplica/metadata"
            )
    }
    fn prepare(
        origin: Origin<'_>,
        account: AccountView<'_>,
        resources: &ResourcePlan,
        concurrency: Concurrency,
        submission: bool,
        read_only: bool,
    ) -> Result<Self, Error> {
        let limits = resources.limits();
        if account.username.len() > 254
            || account.name.len() > MAX_NAME_BYTES
            || account
                .username
                .chars()
                .chain(account.name.chars())
                .any(crate::unicode::is_noncharacter)
            || concurrency.uploads == 0
            || concurrency.requests == 0
            || concurrency.uploads > limits.https_connections
            || concurrency.requests > limits.https_connections
        {
            return Err(Error::Invalid);
        }
        let mut origin_bytes = [0; MAX_DOMAIN_BYTES + 14];
        let mut origin_text = TextBuffer::new(&mut origin_bytes);
        origin
            .write_canonical(&mut origin_text)
            .map_err(|_| Error::Capacity)?;
        let origin = origin_text.as_str().map_err(|_| Error::Invalid)?;
        let mut id_bytes = [0; wire::IdKind::WIRE_BYTES];
        let id = wire::encode_id(account.id, &mut id_bytes).map_err(|_| Error::Invalid)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(MAX_BYTES)
            .map_err(|_| Error::Capacity)?;
        bytes.resize(MAX_BYTES, 0);
        let mut out = TextBuffer::new(&mut bytes);
        out.format(format_args!(
            concat!(
                "{{\"capabilities\":{{\"urn:ietf:params:jmap:core\":{{\"maxSizeUpload\":{}",
                ",\"maxConcurrentUpload\":{}",
                ",\"maxSizeRequest\":{}",
                ",\"maxConcurrentRequests\":{}",
                ",\"maxCallsInRequest\":{}",
                ",\"maxObjectsInGet\":{}",
                ",\"maxObjectsInSet\":{}",
                ",\"collationAlgorithms\":[]}},\"urn:ietf:params:jmap:mail\":{{}},\"{replication}\":{{}}{submission_global}}}",
                ",\"accounts\":{{\"{}\":{{\"name\":",
            ),
            limits.message_bytes,
            concurrency.uploads,
            limits.json_bytes,
            concurrency.requests,
            limits.json_methods,
            limits.objects_per_method,
            limits
                .objects_per_method
                .min(crate::store_fs::MAX_EMAIL_UPDATES)
                .min(crate::store_fs::MAX_MAILBOX_CREATES)
                .min(crate::store_fs::MAX_EMAIL_DESTROYS),
            id,
            replication = crate::jmap_request::REPLICATION,
            submission_global = if submission { ",\"urn:ietf:params:jmap:submission\":{}" } else { "" },
        ))
        .map_err(|_| Error::Capacity)?;
        quoted(&mut out, account.name)?;
        out.format(format_args!(
            concat!(
            ",\"isPersonal\":true,\"isReadOnly\":{read_only},\"accountCapabilities\":{{",
            "\"urn:ietf:params:jmap:core\":{{}},\"urn:ietf:params:jmap:mail\":{{",
            "\"maxMailboxesPerEmail\":null,\"maxMailboxDepth\":null,",
            "\"maxSizeMailboxName\":{},\"maxSizeAttachmentsPerEmail\":{},",
            "\"emailQuerySortOptions\":[\"receivedAt\"],\"mayCreateTopLevelMailbox\":{may_create}}},\"{replication}\":{{\"inventoryBaseFence\":true,\"maxInventoryIds\":{inventory_limit},\"maxMetadataIds\":{metadata_limit}}}{submission_account}}}}}}}",
            ",\"primaryAccounts\":{{\"urn:ietf:params:jmap:mail\":\"{}\""
        ),
            crate::format::row::MAX_MAILBOX_NAME,
            limits.message_bytes,
            id,
            replication = crate::jmap_request::REPLICATION,
            read_only = read_only,
            may_create = !read_only,
            inventory_limit = limits.query_page.min(crate::postgres_read::MAX_REPLICA_INVENTORY_PAGE),
            metadata_limit = limits.objects_per_method.min(crate::postgres_body::MAX_METADATA_IDS),
            submission_account = if submission { ",\"urn:ietf:params:jmap:submission\":{\"maxDelayedSend\":0,\"submissionExtensions\":{}}" } else { "" },
        ))
        .map_err(|_| Error::Capacity)?;
        if submission {
            out.format(format_args!(
                ",\"urn:ietf:params:jmap:submission\":\"{id}\""
            ))
            .map_err(|_| Error::Capacity)?;
        }
        out.append("},\"username\":").map_err(|_| Error::Capacity)?;
        quoted(&mut out, account.username)?;
        for (field, suffix) in [
            ("apiUrl", API_PATH),
            ("downloadUrl", DOWNLOAD_TEMPLATE),
            ("uploadUrl", UPLOAD_TEMPLATE),
            ("eventSourceUrl", EVENT_TEMPLATE),
        ] {
            // Origin is validated ASCII DNS/port syntax; all suffixes are
            // fixed URI templates with no JSON metacharacters.
            out.format(format_args!(",\"{field}\":\"{origin}{suffix}\""))
                .map_err(|_| Error::Capacity)?;
        }
        let mut digest = td_crypto::Sha256::try_new().map_err(Error::from)?;
        digest
            .update(b"td-mta td-mail Session v1\0")
            .map_err(Error::from)?;
        digest
            .update(out.as_str().map_err(|_| Error::Invalid)?.as_bytes())
            .map_err(Error::from)?;
        let mut state = [0; 64];
        wire::hex(&digest.finish().map_err(Error::from)?, &mut state)
            .map_err(|_| Error::Invalid)?;
        let state_text = std::str::from_utf8(&state).map_err(|_| Error::Invalid)?;
        out.format(format_args!(",\"state\":\"{state_text}\"}}"))
            .map_err(|_| Error::Capacity)?;
        let length = out.len();
        bytes.truncate(length);
        let mut retained_origin = String::new();
        retained_origin
            .try_reserve_exact(origin.len())
            .map_err(|_| Error::Capacity)?;
        retained_origin.push_str(origin);
        Ok(Self {
            origin: retained_origin,
            submission,
            read_only,
            bytes,
            state,
            concurrency,
        })
    }
    /// The request parser accepts exactly the capabilities in this document.
    pub fn capabilities(&self) -> &'static [&'static str] {
        use crate::jmap_request::{CORE, MAIL, REPLICATION, SUBMISSION};
        if self.submission {
            &[CORE, MAIL, SUBMISSION, REPLICATION]
        } else {
            &[CORE, MAIL, REPLICATION]
        }
    }
    pub fn origin(&self) -> Result<Origin<'_>, Error> {
        Origin::parse(&self.origin).map_err(|_| Error::Invalid)
    }
    pub fn concurrency(&self) -> Concurrency {
        self.concurrency
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn state(&self) -> Result<&str, Error> {
        std::str::from_utf8(&self.state).map_err(|_| Error::Invalid)
    }
}
fn append(out: &mut TextBuffer<'_>, text: &str) -> Result<(), Error> {
    out.append(text).map_err(|_| Error::Capacity)
}
struct Text<'a>(std::str::Chars<'a>);
impl Source for Text<'_> {
    type Context = ();
    type Error = Error;
    fn charge_output(&mut self, (): (), _: u64) -> Result<(), Error> {
        Ok(())
    }
    fn poll(&mut self, (): ()) -> Result<Scalar, Error> {
        Ok(self.0.next().map(Scalar::Value).unwrap_or(Scalar::Complete))
    }
}
pub(crate) fn quoted(out: &mut TextBuffer<'_>, value: &str) -> Result<(), Error> {
    let mut source = Text(value.chars());
    let mut frame = Frame::new();
    loop {
        // Frame retains at most six bytes per scalar and emits non-ASCII
        // as UTF-8, not surrogate pairs. This window drains each scalar
        // whole; zero-byte source transitions still advance the frame.
        let mut bytes = [0; 6];
        let progress = frame
            .poll(&mut source, (), &mut bytes)
            .map_err(|_| Error::Invalid)?;
        let text = std::str::from_utf8(bytes.get(..progress.written).ok_or(Error::Invalid)?)
            .map_err(|_| Error::Invalid)?;
        append(out, text)?;
        if progress.status == Status::Complete {
            return Ok(());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{ids::AccountId, limits::Limits};
    fn account() -> AccountView<'static> {
        AccountView {
            id: AccountId::from_bytes([7; 16]),
            username: "reader",
            name: "Personal mail",
        }
    }
    fn build(
        origin: &str,
        account: AccountView<'_>,
        limits: Limits,
        uploads: usize,
        requests: usize,
    ) -> Session {
        Session::mail_access(
            Origin::parse(origin).unwrap(),
            account,
            &limits.plan().unwrap(),
            Concurrency { uploads, requests },
        )
        .unwrap()
    }
    #[test]
    fn read_only_session_advertises_and_enforces_closed_read_surface() {
        let session = Session::mail_read_only(
            Origin::parse("https://mail.example.test").unwrap(),
            account(),
            &Limits::default().plan().unwrap(),
            Concurrency {
                uploads: 1,
                requests: 2,
            },
        )
        .unwrap();
        assert!(session.is_read_only());
        assert!(!session
            .capabilities()
            .contains(&crate::jmap_request::SUBMISSION));
        let value = td_json::parse(std::str::from_utf8(session.bytes()).unwrap()).unwrap();
        let accounts = value["accounts"].as_object().unwrap();
        let (_, account) = accounts.first().unwrap();
        assert_eq!(account["isReadOnly"].as_bool(), Some(true));
        assert_eq!(
            account["accountCapabilities"][crate::jmap_request::MAIL]["mayCreateTopLevelMailbox"]
                .as_bool(),
            Some(false)
        );
        for method in [
            b"Email/get".as_slice(),
            b"Mailbox/get",
            b"Email/query",
            b"Email/changes",
            b"TdReplica/metadata",
        ] {
            assert!(session.permits_method(method));
        }
        for method in [
            b"Email/set".as_slice(),
            b"Mailbox/set",
            b"EmailSubmission/set",
            b"Blob/copy",
            b"PushSubscription/set",
            b"Future/mutate",
        ] {
            assert!(!session.permits_method(method));
        }
    }

    #[test]
    fn complete_core_document_uses_only_configured_origin_and_limits() {
        let session = build(
            "https://MAIL.example.test:8443/",
            account(),
            Limits::default(),
            1,
            2,
        );
        let value = td_json::parse(std::str::from_utf8(session.bytes()).unwrap()).unwrap();
        let core = &value["capabilities"]["urn:ietf:params:jmap:core"];
        assert_eq!(core["maxSizeUpload"].as_u64(), Some(32 * 1024 * 1024));
        assert_eq!(core["maxConcurrentUpload"].as_u64(), Some(1));
        assert_eq!(core["maxSizeRequest"].as_u64(), Some(1024 * 1024));
        assert_eq!(core["maxConcurrentRequests"].as_u64(), Some(2));
        assert_eq!(core["maxCallsInRequest"].as_u64(), Some(16));
        assert_eq!(core["maxObjectsInGet"].as_u64(), Some(256));
        assert_eq!(core["maxObjectsInSet"].as_u64(), Some(256));
        assert!(core["collationAlgorithms"].as_array().unwrap().is_empty());
        assert_eq!(value["capabilities"].as_object().unwrap().len(), 3);
        assert_eq!(value["accounts"].as_object().unwrap().len(), 1);
        let object = &value["accounts"]["a07070707070707070707070707070707"];
        assert_eq!(object["name"].as_str(), Some("Personal mail"));
        assert_eq!(object["isPersonal"].as_bool(), Some(true));
        assert_eq!(object["isReadOnly"].as_bool(), Some(false));
        let replication = crate::jmap_request::REPLICATION;
        assert!(session.capabilities().contains(&replication));
        assert!(value["capabilities"][replication].as_object().is_some());
        assert_eq!(
            object["accountCapabilities"][replication],
            td_json::json!({"inventoryBaseFence":true,"maxInventoryIds":256,"maxMetadataIds":16})
        );
        let mail = &object["accountCapabilities"][crate::jmap_request::MAIL];
        assert_eq!(
            mail["maxSizeMailboxName"].as_u64(),
            Some(crate::format::row::MAX_MAILBOX_NAME as u64)
        );
        assert_eq!(mail["mayCreateTopLevelMailbox"].as_bool(), Some(true));
        assert_eq!(
            mail["emailQuerySortOptions"],
            td_json::json!(["receivedAt"])
        );
        assert!(object["accountCapabilities"]["urn:ietf:params:jmap:core"]
            .as_object()
            .is_some());
        assert_eq!(
            value["primaryAccounts"]["urn:ietf:params:jmap:mail"].as_str(),
            Some("a07070707070707070707070707070707")
        );
        assert_eq!(value["username"].as_str(), Some("reader"));
        for (field, suffix) in [
            ("apiUrl", API_PATH),
            ("downloadUrl", DOWNLOAD_TEMPLATE),
            ("uploadUrl", UPLOAD_TEMPLATE),
            ("eventSourceUrl", EVENT_TEMPLATE),
        ] {
            assert_eq!(
                value[field].as_str(),
                Some(format!("https://mail.example.test:8443{suffix}").as_str())
            );
        }
        assert_eq!(value["state"].as_str(), Some(session.state().unwrap()));
        assert_eq!(value.as_object().unwrap().len(), 9);
        assert!(!format!("{session:?}").contains("reader"));
    }
    #[test]
    fn prepared_submission_metadata_changes_capabilities_and_session_state() {
        let limits = Limits::default();
        let read = build("https://mail.test", account(), limits, 1, 2);
        let send = Session::mail_submission(
            Origin::parse("https://mail.test").unwrap(),
            account(),
            &limits.plan().unwrap(),
            Concurrency {
                uploads: 1,
                requests: 2,
            },
        )
        .unwrap();
        let value = td_json::parse_slice(send.bytes()).unwrap();
        let capability = crate::jmap_request::SUBMISSION;
        assert!(send.capabilities().contains(&capability));
        assert!(!read.capabilities().contains(&capability));
        assert!(value["capabilities"].get(capability).is_some());
        let id = "a07070707070707070707070707070707";
        let settings = &value["accounts"][id]["accountCapabilities"][capability];
        assert_eq!(settings["maxDelayedSend"].as_u64(), Some(0));
        assert!(settings["submissionExtensions"]
            .as_object()
            .unwrap()
            .is_empty());
        assert_eq!(value["primaryAccounts"][capability].as_str(), Some(id));
        assert_ne!(read.state().unwrap(), send.state().unwrap());
        let read = td_json::parse_slice(read.bytes()).unwrap();
        assert!(read["capabilities"].get(capability).is_none());
    }
    #[test]
    fn state_tracks_all_variable_metadata_and_canonical_origin() {
        let limits = Limits::default();
        let original = build("https://mail.test", account(), limits, 1, 2);
        let equivalent = build("https://MAIL.test:443/", account(), limits, 1, 2);
        assert_eq!(original.bytes(), equivalent.bytes());
        let variants = [
            build("https://other.test", account(), limits, 1, 2),
            build("https://mail.test:444", account(), limits, 1, 2),
            build(
                "https://mail.test",
                AccountView {
                    id: AccountId::from_bytes([8; 16]),
                    ..account()
                },
                limits,
                1,
                2,
            ),
            build(
                "https://mail.test",
                AccountView {
                    name: "Other",
                    ..account()
                },
                limits,
                1,
                2,
            ),
            build(
                "https://mail.test",
                AccountView {
                    username: "other",
                    ..account()
                },
                limits,
                1,
                2,
            ),
            build("https://mail.test", account(), limits, 2, 2),
            build("https://mail.test", account(), limits, 1, 3),
            build(
                "https://mail.test",
                account(),
                Limits {
                    message_bytes: 16 * 1024 * 1024,
                    ..limits
                },
                1,
                2,
            ),
            build(
                "https://mail.test",
                account(),
                Limits {
                    json_bytes: 2048,
                    ..limits
                },
                1,
                2,
            ),
            build(
                "https://mail.test",
                account(),
                Limits {
                    json_methods: 8,
                    ..limits
                },
                1,
                2,
            ),
            build(
                "https://mail.test",
                account(),
                Limits {
                    objects_per_method: 32,
                    ..limits
                },
                1,
                2,
            ),
        ];
        for variant in variants {
            assert_ne!(original.state().unwrap(), variant.state().unwrap());
        }
    }
    #[test]
    fn maximum_escaped_identity_fits_and_invalid_bounds_refuse() {
        let name = "\u{1}".repeat(MAX_NAME_BYTES);
        let username = "\u{1}".repeat(254);
        let account = AccountView {
            name: &name,
            username: &username,
            ..account()
        };
        let session = build("https://mail.test", account, Limits::default(), 1, 1);
        let value = td_json::parse(std::str::from_utf8(session.bytes()).unwrap()).unwrap();
        assert_eq!(value["username"].as_str(), Some(username.as_str()));
        assert_eq!(
            value["accounts"].as_object().unwrap()[0].1["name"].as_str(),
            Some(name.as_str())
        );
        assert!(session.bytes().len() < MAX_BYTES);
        let plan = Limits::default().plan().unwrap();
        let over = plan.limits().https_connections + 1;
        for (uploads, requests) in [(0, 1), (1, 0), (over, 1), (1, over)] {
            assert_eq!(
                Session::mail_access(
                    Origin::parse("https://mail.test").unwrap(),
                    account,
                    &plan,
                    Concurrency { uploads, requests }
                )
                .unwrap_err(),
                Error::Invalid
            );
        }
        for name in [
            "x".repeat(MAX_NAME_BYTES + 1),
            "\u{fdd0}".into(),
            "\u{10ffff}".into(),
        ] {
            assert_eq!(
                Session::mail_access(
                    Origin::parse("https://mail.test").unwrap(),
                    AccountView {
                        name: &name,
                        ..account
                    },
                    &plan,
                    Concurrency {
                        uploads: 1,
                        requests: 1
                    }
                )
                .unwrap_err(),
                Error::Invalid
            );
        }
    }
    #[test]
    fn unicode_names_and_maximum_origin_round_trip() {
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(MAX_DOMAIN_BYTES - 192)
        );
        let origin = format!("https://{host}:65535");
        let name = "Zoë 名 📬";
        let session = build(
            &origin,
            AccountView { name, ..account() },
            Limits::default(),
            1,
            2,
        );
        let value = td_json::parse(std::str::from_utf8(session.bytes()).unwrap()).unwrap();
        assert_eq!(
            value["accounts"].as_object().unwrap()[0].1["name"].as_str(),
            Some(name)
        );
        assert_eq!(
            value["apiUrl"].as_str(),
            Some(format!("{origin}{API_PATH}").as_str())
        );
        assert_eq!(
            session.concurrency(),
            Concurrency {
                uploads: 1,
                requests: 2
            }
        );
        let largest = "\u{1}".repeat(MAX_NAME_BYTES);
        let username = "\u{1}".repeat(254);
        let session = build(
            &origin,
            AccountView {
                name: &largest,
                username: &username,
                ..account()
            },
            Limits::default(),
            1,
            2,
        );
        assert!(session.bytes().len() < MAX_BYTES);
        for username in ["x".repeat(255), "\u{fdd0}".into()] {
            assert_eq!(
                Session::mail_access(
                    Origin::parse(&origin).unwrap(),
                    AccountView {
                        username: &username,
                        ..account()
                    },
                    &Limits::default().plan().unwrap(),
                    Concurrency {
                        uploads: 1,
                        requests: 2
                    }
                )
                .unwrap_err(),
                Error::Invalid
            );
        }
    }
}

#[cfg(test)]
mod flag_profile_tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn flag_profile_is_writable_and_caps_set_batch_independently_of_get() {
        let resources = crate::limits::Limits {
            objects_per_method: 512,
            ..crate::limits::Limits::default()
        }
        .plan()
        .unwrap();
        let session = Session::mail_access(
            Origin::parse("https://mail.example").unwrap(),
            AccountView {
                id: crate::ids::AccountId::from_bytes([1; 16]),
                username: "user",
                name: "User",
            },
            &resources,
            Concurrency {
                uploads: 1,
                requests: 1,
            },
        )
        .unwrap();
        let value = td_json::parse(std::str::from_utf8(session.bytes()).unwrap()).unwrap();
        let core = &value["capabilities"]["urn:ietf:params:jmap:core"];
        assert_eq!(core["maxObjectsInGet"].as_u64(), Some(512));
        assert_eq!(core["maxObjectsInSet"].as_u64(), Some(256));
        let id = wire::encode_id(
            crate::ids::AccountId::from_bytes([1; 16]),
            &mut [0; wire::IdKind::WIRE_BYTES],
        )
        .unwrap()
        .to_owned();
        assert_eq!(
            value["accounts"][id.as_str()]["isReadOnly"].as_bool(),
            Some(false)
        );
    }
}
