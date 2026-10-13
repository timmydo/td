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
const MAX_BYTES: usize = 32 * 1024;

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
                ",\"collationAlgorithms\":[]}},\"urn:ietf:params:jmap:mail\":{{}}}}",
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
                .min(crate::store_fs::MAX_MAILBOX_CREATES),
            id,
        ))
        .map_err(|_| Error::Capacity)?;
        quoted(&mut out, account.name)?;
        out.format(format_args!(
            concat!(
            ",\"isPersonal\":true,\"isReadOnly\":false,\"accountCapabilities\":{{",
            "\"urn:ietf:params:jmap:core\":{{}},\"urn:ietf:params:jmap:mail\":{{",
            "\"maxMailboxesPerEmail\":null,\"maxMailboxDepth\":null,",
            "\"maxSizeMailboxName\":{},\"maxSizeAttachmentsPerEmail\":{},",
            "\"emailQuerySortOptions\":[\"receivedAt\"],\"mayCreateTopLevelMailbox\":true}}}}}}}}",
            ",\"primaryAccounts\":{{\"urn:ietf:params:jmap:mail\":\"{}\"}},\"username\":"
        ),
            crate::format::row::MAX_MAILBOX_NAME,
            limits.message_bytes,
            id
        ))
        .map_err(|_| Error::Capacity)?;
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
        Ok(Self {
            bytes,
            state,
            concurrency,
        })
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
        assert_eq!(value["capabilities"].as_object().unwrap().len(), 2);
        assert_eq!(value["accounts"].as_object().unwrap().len(), 1);
        let object = &value["accounts"]["a07070707070707070707070707070707"];
        assert_eq!(object["name"].as_str(), Some("Personal mail"));
        assert_eq!(object["isPersonal"].as_bool(), Some(true));
        assert_eq!(object["isReadOnly"].as_bool(), Some(false));
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
