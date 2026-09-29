//! Read-only operator-file inventory; requests are not file-trust evidence.
use super::{certificate, gateway, identity, material, storage};
use crate::ids::IdentityId;
use std::{convert::Infallible, fmt, num::NonZeroU64};

const IDENTITY_END: usize = 2 * identity::MAX_IDENTITIES;
const CERTIFICATE_START: usize = IDENTITY_END + 3;
const GATEWAY_START: usize = CERTIFICATE_START + 2 * certificate::MAX_PROFILES;
/// Conservative scan ceiling; some optional slots are mutually exclusive.
pub const MAX_SLOTS: usize = GATEWAY_START + gateway::MAX_GATEWAYS;
pub const MAX_CHAIN_BYTES: usize = 64 * 1024;
pub const MAX_KEY_BYTES: usize = 16 * 1024;
pub const MAX_CA_BYTES: usize = 128 * 1024;

/// Indices identify profiles within this candidate only; no runtime authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    TextSignature(IdentityId),
    HtmlSignature(IdentityId),
    RelayPassword,
    RelayCa,
    AcmeCa,
    CertificateChain(u8),
    CertificateKey(u8),
    GatewayCa(u8),
}
impl Target {
    /// Raw file bytes, excluding the extra byte needed to detect overflow.
    pub const fn maximum_bytes(self) -> usize {
        match self {
            Self::TextSignature(_) | Self::HtmlSignature(_) => material::MAX_SIGNATURE_BYTES,
            Self::RelayPassword => material::PASSWORD_SCRATCH_BYTES - 1,
            Self::CertificateChain(_) => MAX_CHAIN_BYTES,
            Self::CertificateKey(_) => MAX_KEY_BYTES,
            Self::RelayCa | Self::AcmeCa | Self::GatewayCa(_) => MAX_CA_BYTES,
        }
    }
    /// These roles additionally require no group/other permissions in M05.
    pub const fn requires_private_mode(self) -> bool {
        matches!(self, Self::RelayPassword | Self::CertificateKey(_))
    }
    pub const fn material_kind(self) -> Option<material::Kind> {
        match self {
            Self::TextSignature(_) | Self::HtmlSignature(_) => Some(material::Kind::Signature),
            Self::RelayPassword => Some(material::Kind::RelayPassword),
            _ => None,
        }
    }
}
pub struct Reference<'a> {
    target: Target,
    path: &'a str,
}
impl fmt::Debug for Reference<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationFileReference(<redacted>)")
    }
}
impl<'a> Reference<'a> {
    pub fn target(&self) -> Target {
        self.target
    }
    /// Explicit access for the trusted filesystem adapter, never diagnostics.
    pub fn path(&self) -> &'a str {
        self.path
    }
}
pub enum Error<E = Infallible> {
    ForeignCandidate,
    FailedCursor,
    Invariant,
    Callback(E),
}
impl<E> Error<E> {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ForeignCandidate => "config_inputs_foreign_candidate",
            Self::FailedCursor => "config_inputs_failed_cursor",
            Self::Invariant => "config_inputs_invariant",
            Self::Callback(_) => "config_inputs_callback",
        }
    }
}
impl<E> fmt::Debug for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl<E> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl<E> std::error::Error for Error<E> {}

/// No long-lived candidate borrow: release each callback before finalization
/// appends content. The same owner must be supplied on every operation.
pub struct Cursor {
    owner: NonZeroU64,
    slot: usize,
    failed: bool,
}
impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationInputCursor(<redacted>)")
    }
}
impl Cursor {
    pub fn new(candidate: &storage::Candidate) -> Result<Self, Error> {
        Ok(Self {
            owner: candidate.input_owner().map_err(|_| Error::Invariant)?,
            slot: 0,
            failed: false,
        })
    }
    /// Calls the visitor at most once, skipping absent optional references.
    /// None means inventory exhaustion, not successful reads or validation.
    /// A callback may return values borrowed from its own buffers, but cannot
    /// retain the short-lived Reference. Any failure makes this cursor terminal.
    /// Callback allocation, blocking and side effects remain caller obligations.
    /// A path cannot be returned from the callback:
    /// ```compile_fail
    /// use td_mta::config::{inputs::Cursor, storage::Candidate};
    /// fn escape(candidate: &Candidate) {
    ///     let mut cursor = Cursor::new(candidate).unwrap();
    ///     let _ = cursor.visit_next(candidate, |input| Ok::<_, ()>(input.path()));
    /// }
    /// ```
    /// Nor can it be stored in an outer slot:
    /// ```compile_fail,E0521
    /// use td_mta::config::{inputs::Cursor, storage::Candidate};
    /// fn retain(candidate: &Candidate) {
    ///     let mut cursor = Cursor::new(candidate).unwrap();
    ///     let mut saved = None;
    ///     let _ = cursor.visit_next(candidate, |input| {
    ///         saved = Some(input.path());
    ///         Ok::<_, ()>(())
    ///     });
    ///     let _ = saved;
    /// }
    /// ```
    pub fn visit_next<R, E>(
        &mut self,
        candidate: &storage::Candidate,
        visit: impl FnOnce(Reference<'_>) -> Result<R, E>,
    ) -> Result<Option<R>, Error<E>> {
        if self.failed {
            return Err(Error::FailedCursor);
        }
        // A caught callback unwind must not resume after skipping its input.
        self.failed = true;
        let result = self.next(candidate, visit);
        if result.is_ok() {
            self.failed = false;
        }
        result
    }
    fn next<R, E>(
        &mut self,
        candidate: &storage::Candidate,
        visit: impl FnOnce(Reference<'_>) -> Result<R, E>,
    ) -> Result<Option<R>, Error<E>> {
        if candidate.input_owner().map_err(|_| Error::Invariant)? != self.owner {
            return Err(Error::ForeignCandidate);
        }
        let mut visit = Some(visit);
        while self.slot < MAX_SLOTS {
            let slot = self.slot;
            self.slot = self.slot.checked_add(1).ok_or(Error::Invariant)?;
            if let Some(value) = at(candidate, slot, &mut visit)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
}
fn emit<R, E>(
    path: Option<&str>,
    target: Target,
    visit: &mut Option<impl FnOnce(Reference<'_>) -> Result<R, E>>,
) -> Result<Option<R>, Error<E>> {
    let Some(path) = path else { return Ok(None) };
    let visit = visit.take().ok_or(Error::Invariant)?;
    visit(Reference { target, path })
        .map(Some)
        .map_err(Error::Callback)
}
fn at<R, E>(
    candidate: &storage::Candidate,
    slot: usize,
    visit: &mut Option<impl FnOnce(Reference<'_>) -> Result<R, E>>,
) -> Result<Option<R>, Error<E>> {
    if slot < IDENTITY_END {
        let identities = candidate.identities().map_err(|_| Error::Invariant)?;
        let Some(identity) = identities
            .identity(slot / 2)
            .map_err(|_| Error::Invariant)?
        else {
            return Ok(None);
        };
        return if slot.is_multiple_of(2) {
            emit(
                identity.text_signature_file,
                Target::TextSignature(identity.id),
                visit,
            )
        } else {
            emit(
                identity.html_signature_file,
                Target::HtmlSignature(identity.id),
                visit,
            )
        };
    }
    if slot == IDENTITY_END || slot == IDENTITY_END + 1 {
        let outbound = candidate.outbound().map_err(|_| Error::Invariant)?;
        let relay = outbound.relay().map_err(|_| Error::Invariant)?;
        return if slot == IDENTITY_END {
            emit(Some(relay.password_file), Target::RelayPassword, visit)
        } else {
            emit(relay.ca_file, Target::RelayCa, visit)
        };
    }
    candidate
        .with_graph(|records, text| {
            let graph = records.view(text).map_err(|_| Error::Invariant)?;
            if slot == IDENTITY_END + 2 {
                let certificates = graph.certificates().map_err(|_| Error::Invariant)?;
                let acme = certificates.acme().map_err(|_| Error::Invariant)?;
                return emit(acme.and_then(|value| value.ca_file), Target::AcmeCa, visit);
            }
            if slot < GATEWAY_START {
                let offset = slot
                    .checked_sub(CERTIFICATE_START)
                    .ok_or(Error::Invariant)?;
                let index = offset / 2;
                let certificates = graph.certificates().map_err(|_| Error::Invariant)?;
                let Some(profile) = certificates.profile(index).map_err(|_| Error::Invariant)?
                else {
                    return Ok(None);
                };
                let index = u8::try_from(index).map_err(|_| Error::Invariant)?;
                return if offset.is_multiple_of(2) {
                    emit(profile.chain_file, Target::CertificateChain(index), visit)
                } else {
                    emit(profile.key_file, Target::CertificateKey(index), visit)
                };
            }
            let index = slot.checked_sub(GATEWAY_START).ok_or(Error::Invariant)?;
            let gateways = graph.gateways().map_err(|_| Error::Invariant)?;
            let Some(gateway) = gateways.gateway(index).map_err(|_| Error::Invariant)? else {
                return Ok(None);
            };
            let index = u8::try_from(index).map_err(|_| Error::Invariant)?;
            emit(Some(gateway.ca_file), Target::GatewayCa(index), visit)
        })
        .map_err(|_| Error::Invariant)?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::{load, stanza::Pending, stream, text};
    const BASE: &str = r#"version = 1
[server]
hostname = "mail.example.test"
jmap_origin = "https://jmap.example.test"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "private-user"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "relay.example.test"
port = 465
username = "private-relay"
password_file = "/private-password"
[certificate "public"]
mode = "files"
chain_file = "/private-chain"
key_file = "/private-key"
[listener "smtp"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "mail.example.test"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "0.0.0.0:443"
certificate = "public"
"#;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        text::TEST_CONSTRUCTION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn parse(source: &str, storage: storage::Storage) -> storage::Candidate {
        let result = load::read(
            storage,
            &mut Pending::new(),
            &mut vec![0; stream::SCRATCH_BYTES],
            &mut source.as_bytes(),
        );
        if let Err(failed) = &result {
            eprintln!("{} {:?}", failed.error(), failed.error());
        }
        result.unwrap().into_candidate()
    }
    fn candidate(source: &str) -> storage::Candidate {
        parse(source, storage::Storage::try_new().unwrap())
    }
    fn inventory(candidate: &storage::Candidate) -> Vec<(Target, String)> {
        let mut cursor = Cursor::new(candidate).unwrap();
        let mut found = Vec::new();
        while let Some(item) = cursor
            .visit_next(candidate, |input| {
                Ok::<_, ()>((input.target(), input.path().to_owned()))
            })
            .unwrap()
        {
            found.push(item);
        }
        assert!(cursor
            .visit_next(candidate, |_| Err::<(), _>("must not run"))
            .unwrap()
            .is_none());
        assert_eq!(cursor.slot, MAX_SLOTS);
        found
    }
    #[test]
    fn defaults_skip_absent_inputs_and_preserve_distinct_roles_for_shared_paths() {
        let _lock = lock();
        let base = candidate(BASE);
        assert_eq!(
            inventory(&base),
            vec![
                (Target::RelayPassword, "/private-password".into()),
                (Target::CertificateChain(0), "/private-chain".into()),
                (Target::CertificateKey(0), "/private-key".into()),
            ]
        );
        let source = BASE.replace("[resolver \"primary\"]", "text_signature_file = \"/same\"\nhtml_signature_file = \"/same\"\n[resolver \"primary\"]")
            .replace("/private-password", "/same");
        // Inventory preserves roles; M05 must later reject secret/public aliasing.
        let id = IdentityId::from_bytes([0xbb; 16]);
        let found = inventory(&candidate(&source));
        assert_eq!(
            &found[..3],
            &[
                (Target::TextSignature(id), "/same".into()),
                (Target::HtmlSignature(id), "/same".into()),
                (Target::RelayPassword, "/same".into()),
            ]
        );
        assert_eq!(found.len(), 5);
    }
    #[test]
    fn acme_and_unused_gateway_trust_are_included_without_managed_keys() {
        let _lock = lock();
        let source =
            BASE.replace(
                "mode = \"files\"\nchain_file = \"/private-chain\"\nkey_file = \"/private-key\"",
                "mode = \"acme\"",
            )
            .replace(
                "password_file = \"/private-password\"",
                "password_file = \"/private-password\"\nca_file = \"/same-ca\"",
            ) + &format!(
                r#"[acme]
directory = "https://ca.example.test/directory"
contact = "main@example.test"
terms_accepted = true
ca_file = "/same-ca"
[listener "challenge"]
kind = "http01"
bind = "0.0.0.0:80"
[gateway "unused"]
ca_file = "/same-ca"
client_cert_sha256 = "{}"
"#,
                "a".repeat(64)
            );
        assert_eq!(
            inventory(&candidate(&source)),
            vec![
                (Target::RelayPassword, "/private-password".into()),
                (Target::RelayCa, "/same-ca".into()),
                (Target::AcmeCa, "/same-ca".into()),
                (Target::GatewayCa(0), "/same-ca".into()),
            ]
        );
    }
    #[test]
    fn full_tables_fit_the_scan_ceiling_and_keep_identity_order() {
        let _lock = lock();
        let mut source = BASE.replace("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "00000000000000000000000000000001")
            .replace("[resolver \"primary\"]", "text_signature_file = \"/signature\"\nhtml_signature_file = \"/signature\"\n[resolver \"primary\"]")
            .replace("password_file = \"/private-password\"", "password_file = \"/private-password\"\nca_file = \"/ca\"")
            .replace("[listener \"https\"]\nkind = \"https\"\nbind = \"0.0.0.0:443\"\ncertificate = \"public\"", "[listener \"https\"]\nkind = \"https\"\nbind = \"0.0.0.0:443\"\ncertificate = \"p1\"");
        source = source.replace("0.0.0.0:443", "127.0.0.1:443");
        for n in (2..=64).rev() {
            source.push_str(&format!(
                r#"[identity "{n:032x}"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
text_signature_file = "/signature"
html_signature_file = "/signature"
"#
            ));
        }
        for n in 1..16 {
            source.push_str(&format!(
                r#"[certificate "p{n}"]
mode = "files"
chain_file = "/chain/{n}"
key_file = "/key/{n}"
"#
            ));
            if n >= 2 {
                source.push_str(&format!(
                    r#"[listener "extra{n}"]
kind = "https"
bind = "127.0.0.{n}:443"
certificate = "p{n}"
"#
                ));
            }
        }
        for n in 0..16 {
            source.push_str(&format!(
                r#"[gateway "g{n}"]
ca_file = "/ca/{n}"
client_cert_sha256 = "{}"
"#,
                "a".repeat(64)
            ));
        }
        let found = inventory(&candidate(&source));
        assert_eq!(MAX_SLOTS, 179);
        assert_eq!(found.len(), 178); // No ACME CA when every profile uses files.
        for n in 1..=64usize {
            let id = IdentityId::from_bytes((n as u128).to_be_bytes());
            assert_eq!(found[(n - 1) * 2].0, Target::TextSignature(id));
            assert_eq!(found[(n - 1) * 2 + 1].0, Target::HtmlSignature(id));
        }
        assert_eq!(found[128].0, Target::RelayPassword);
        assert_eq!(found[129].0, Target::RelayCa);
        assert_eq!(found[161].0, Target::CertificateKey(15));
        assert_eq!(found.last().unwrap().0, Target::GatewayCa(15));
    }
    #[test]
    fn cursor_rejects_rebuilt_storage_and_callback_values_borrow_only_caller_buffers() {
        let _lock = lock();
        let candidate = candidate(BASE);
        let mut cursor = Cursor::new(&candidate).unwrap();
        let moved = candidate;
        let mut scratch = vec![0; material::PASSWORD_SCRATCH_BYTES];
        let value = cursor
            .visit_next(&moved, |input| {
                assert_eq!(input.target(), Target::RelayPassword);
                material::read(
                    input.target().material_kind().unwrap(),
                    &mut b"password\n".as_slice(),
                    &mut scratch,
                )
            })
            .unwrap()
            .unwrap();
        let storage = moved.into_storage();
        assert_eq!(value.text(), "password");
        let rebuilt = parse(BASE, storage);
        let mut calls = 0;
        let error = cursor
            .visit_next(&rebuilt, |_| {
                calls += 1;
                Ok::<_, ()>(())
            })
            .unwrap_err();
        assert!(matches!(error, Error::ForeignCandidate));
        assert_eq!(calls, 0);
        assert!(matches!(
            cursor.visit_next(&rebuilt, |_| Ok::<_, ()>(())),
            Err(Error::FailedCursor)
        ));
        assert_eq!(inventory(&rebuilt).len(), 3);
    }
    #[test]
    fn callback_failure_is_terminal_and_diagnostics_never_include_paths_or_payloads() {
        let _lock = lock();
        let candidate = candidate(BASE);
        let mut cursor = Cursor::new(&candidate).unwrap();
        let error = cursor
            .visit_next(&candidate, |input| {
                assert_eq!(
                    format!("{input:?}"),
                    "ConfigurationFileReference(<redacted>)"
                );
                Err::<(), _>("private-password /private-path")
            })
            .unwrap_err();
        assert_eq!(
            format!("{error:?} {error}"),
            "config_inputs_callback config_inputs_callback"
        );
        assert!(std::error::Error::source(&error).is_none());
        assert!(matches!(
            error,
            Error::Callback("private-password /private-path")
        ));
        assert!(matches!(
            cursor.visit_next(&candidate, |_| Ok::<_, ()>(())),
            Err(Error::FailedCursor)
        ));
        assert_eq!(
            format!("{cursor:?}"),
            "ConfigurationInputCursor(<redacted>)"
        );
    }
    #[test]
    fn graph_callback_failure_retains_its_type_and_poisoning() {
        let _lock = lock();
        let candidate = candidate(BASE);
        let mut cursor = Cursor::new(&candidate).unwrap();
        assert_eq!(
            cursor
                .visit_next(&candidate, |input| Ok::<_, u8>(input.target()))
                .unwrap(),
            Some(Target::RelayPassword)
        );
        assert!(matches!(
            cursor.visit_next(&candidate, |input| {
                assert_eq!(input.target(), Target::CertificateChain(0));
                Err::<(), _>(17u8)
            }),
            Err(Error::Callback(17))
        ));
        assert!(matches!(
            cursor.visit_next(&candidate, |_| Ok::<_, u8>(())),
            Err(Error::FailedCursor)
        ));
    }
    #[test]
    #[allow(clippy::panic)]
    fn caught_callback_panic_leaves_cursor_terminal() {
        let _lock = lock();
        let candidate = candidate(BASE);
        let mut cursor = Cursor::new(&candidate).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = cursor.visit_next(&candidate, |_| -> Result<(), ()> {
                panic!("fixture callback panic");
            });
        }));
        assert!(result.is_err());
        let mut called = false;
        assert!(matches!(
            cursor.visit_next(&candidate, |_| {
                called = true;
                Ok::<_, ()>(())
            }),
            Err(Error::FailedCursor)
        ));
        assert!(!called);
    }
    #[test]
    fn raw_limits_and_private_modes_match_each_input_role() {
        let id = IdentityId::from_bytes([1; 16]);
        for (target, maximum, private, kind) in [
            (
                Target::TextSignature(id),
                16384,
                false,
                Some(material::Kind::Signature),
            ),
            (
                Target::HtmlSignature(id),
                16384,
                false,
                Some(material::Kind::Signature),
            ),
            (
                Target::RelayPassword,
                1026,
                true,
                Some(material::Kind::RelayPassword),
            ),
            (Target::CertificateChain(0), 65536, false, None),
            (Target::CertificateKey(0), 16384, true, None),
            (Target::RelayCa, 131072, false, None),
            (Target::AcmeCa, 131072, false, None),
            (Target::GatewayCa(0), 131072, false, None),
        ] {
            assert_eq!(target.maximum_bytes(), maximum);
            assert_eq!(target.requires_private_mode(), private);
            assert_eq!(target.material_kind(), kind);
        }
        assert!(std::mem::size_of::<Cursor>() <= 32);
    }
}
