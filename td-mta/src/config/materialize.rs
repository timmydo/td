//! Exclusive text-input loading; not filesystem trust or runtime publication.
use super::{inputs, load, material, storage, text};
use std::{fmt, io};

pub enum Error<E> {
    Scratch,
    Inventory,
    Input(E),
    Content(material::Error),
    Arena(text::Code),
    Invariant,
}
impl<E> Error<E> {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Scratch => "config_materialize_scratch",
            Self::Inventory => "config_materialize_inventory",
            Self::Input(_) => "config_materialize_input",
            Self::Content(_) => "config_materialize_content",
            Self::Arena(_) => "config_materialize_arena",
            Self::Invariant => "config_materialize_invariant",
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

pub struct Failure<E> {
    storage: storage::Storage,
    error: Error<E>,
    target: Option<inputs::Target>,
}
impl<E> fmt::Debug for Failure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TextInputFailure(<redacted>)")
    }
}
impl<E> Failure<E> {
    pub fn error(&self) -> &Error<E> {
        &self.error
    }
    /// The active input role, without its path; absent for whole-stage errors.
    pub fn target(&self) -> Option<inputs::Target> {
        self.target
    }
    /// Storage may retain input bytes, but has no validated headers.
    pub fn into_parts(self) -> (storage::Storage, Error<E>) {
        (self.storage, self.error)
    }
}

/// Whole-reader structural closure plus decoded text inputs only.
/// This cannot be constructed from a statement-only Candidate:
/// ```compile_fail,E0451
/// use td_mta::config::{materialize::ResolvedText, storage::Candidate, text::Handle};
/// fn forge(candidate: Candidate, password: Handle) -> ResolvedText {
///     ResolvedText { candidate, password }
/// }
/// ```
pub struct ResolvedText {
    candidate: storage::Candidate,
    password: text::Handle,
}
impl fmt::Debug for ResolvedText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedConfigurationText(<redacted>)")
    }
}
impl ResolvedText {
    pub fn candidate(&self) -> &storage::Candidate {
        &self.candidate
    }
    /// Trusted control-worker access only; no authentication or output authority.
    pub fn relay_password(&self) -> Result<&str, text::Code> {
        self.candidate.material_text(self.password)
    }
    /// Missing signature files produce empty strings; indices use raw-ID order.
    pub fn signatures(&self, index: usize) -> Result<Option<Signatures<'_>>, text::Code> {
        self.candidate
            .signature_text(index)
            .map(|value| value.map(|(text, html)| Signatures { text, html }))
    }
    pub fn allocated_bytes(&self) -> Result<usize, storage::Error> {
        self.candidate.materialized_allocated_bytes()
    }
    /// Erases all structural and resolved-input headers; no secure erasure claim.
    pub fn into_storage(self) -> storage::Storage {
        self.candidate.into_storage()
    }
}
pub struct Signatures<'a> {
    pub text: &'a str,
    pub html: &'a str,
}
impl fmt::Debug for Signatures<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedSignatures(<redacted>)")
    }
}

// Clear only the bounded decoder window, including on a caught unwind.
struct ClearScratch<'a>(&'a mut [u8]);
impl Drop for ClearScratch<'_> {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Reuses the stream scratch after source EOF. The opener is called only for
/// signatures and the relay password and returns an owned reader. Its I/O,
/// allocations and file-trust checks remain trusted adapter obligations.
/// The bounded decoder window is cleared on return or unwind; this is
/// observable scratch hygiene, not a secure-erasure guarantee.
/// Provider inputs are not opened. Before production publication M05 must
/// validate all input descriptors and secret/public inode separation, and M07
/// must validate provider material. Nothing here grants those authorities.
#[allow(clippy::result_large_err)]
pub fn read_text<R: io::Read, E>(
    loaded: load::Loaded,
    scratch: &mut [u8],
    mut open: impl FnMut(inputs::Reference<'_>) -> Result<R, E>,
) -> Result<ResolvedText, Failure<E>> {
    let mut candidate = loaded.into_candidate();
    let mut active_target = None;
    let result = (|| {
        let scratch = ClearScratch(
            scratch
                .get_mut(..material::SIGNATURE_SCRATCH_BYTES)
                .ok_or(Error::Scratch)?,
        );
        let mut cursor = inputs::Cursor::new(&candidate).map_err(|_| Error::Inventory)?;
        let mut password = None;
        loop {
            active_target = None;
            let next = cursor
                .visit_next(&candidate, |reference| {
                    let target = reference.target();
                    let Some(kind) = target.material_kind() else {
                        return Ok(None);
                    };
                    active_target = Some(target);
                    let mut reader = open(reference).map_err(Error::Input)?;
                    material::read(kind, &mut reader, scratch.0)
                        .map(|value| Some((target, value)))
                        .map_err(Error::Content)
                })
                .map_err(|error| match error {
                    inputs::Error::Callback(error) => error,
                    _ => Error::Inventory,
                })?;
            let Some(next) = next else { break };
            let Some((target, value)) = next else {
                continue;
            };
            let handle = candidate
                .append_material(value.text())
                .map_err(Error::Arena)?;
            match target {
                inputs::Target::TextSignature(id) => candidate
                    .store_signature(id, false, handle)
                    .map_err(Error::Arena)?,
                inputs::Target::HtmlSignature(id) => candidate
                    .store_signature(id, true, handle)
                    .map_err(Error::Arena)?,
                inputs::Target::RelayPassword => {
                    if password.replace(handle).is_some() {
                        return Err(Error::Invariant);
                    }
                }
                _ => return Err(Error::Invariant),
            }
        }
        candidate
            .signatures_complete()
            .map_err(|_| Error::Invariant)?;
        password.ok_or(Error::Invariant)
    })();
    match result {
        Ok(password) => Ok(ResolvedText {
            candidate,
            password,
        }),
        Err(error) => Err(Failure {
            storage: candidate.into_storage(),
            error,
            target: active_target,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::{stanza::Pending, stream};
    use std::mem::size_of;
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
    fn loaded(source: &str, storage: storage::Storage) -> load::Loaded {
        load::read(
            storage,
            &mut Pending::new(),
            &mut vec![0; stream::SCRATCH_BYTES],
            &mut source.as_bytes(),
        )
        .unwrap()
    }
    fn fresh(source: &str) -> load::Loaded {
        loaded(source, storage::Storage::try_new().unwrap())
    }
    fn with_signatures() -> String {
        BASE.replace("[resolver \"primary\"]", "text_signature_file = \"/text\"\nhtml_signature_file = \"/html\"\n[resolver \"primary\"]")
    }
    #[test]
    fn exact_bytes_defaults_and_provider_inputs_stay_separate() {
        let _lock = lock();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let source = with_signatures();
        let input = load::read(
            storage::Storage::try_new().unwrap(),
            &mut Pending::new(),
            &mut scratch,
            &mut source.as_bytes(),
        )
        .unwrap();
        let before = input.candidate().allocated_bytes().unwrap();
        let mut calls = Vec::new();
        let resolved = read_text(input, &mut scratch, |reference| {
            calls.push((reference.target(), reference.path().to_owned()));
            let bytes: &'static [u8] = match reference.target() {
                inputs::Target::TextSignature(_) => "\u{feff} Hello é\r\n".as_bytes(),
                inputs::Target::HtmlSignature(_) => b"<p> Hello </p>\n",
                inputs::Target::RelayPassword => b" pass word \r\n",
                _ => panic!("provider input opened"),
            };
            Ok::<_, ()>(bytes)
        })
        .unwrap();
        assert_eq!(
            calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(),
            ["/text", "/html", "/private-password"]
        );
        let signatures = resolved.signatures(0).unwrap().unwrap();
        assert_eq!(signatures.text, "\u{feff} Hello é\r\n");
        assert_eq!(signatures.html, "<p> Hello </p>\n");
        assert_eq!(resolved.relay_password().unwrap(), " pass word ");
        assert!(resolved.signatures(1).unwrap().is_none());
        assert!(resolved.signatures(usize::MAX).unwrap().is_none());
        assert_eq!(
            resolved.allocated_bytes().unwrap(),
            before - size_of::<storage::Candidate>() + size_of::<ResolvedText>()
        );
        assert_eq!(
            format!("{resolved:?} {signatures:?}"),
            "ResolvedConfigurationText(<redacted>) ResolvedSignatures(<redacted>)"
        );
        let storage = resolved.into_storage();
        let rebuilt = read_text(
            loaded(BASE, storage),
            &mut vec![0; stream::SCRATCH_BYTES],
            |_| Ok::<_, ()>(b"other".as_slice()),
        )
        .unwrap();
        let signatures = rebuilt.signatures(0).unwrap().unwrap();
        assert_eq!((signatures.text, signatures.html), ("", ""));
        assert_eq!(rebuilt.relay_password().unwrap(), "other");
    }
    #[test]
    fn late_open_failure_erases_headers_and_preserves_separate_generation() {
        let _lock = lock();
        let old = read_text(fresh(BASE), &mut vec![0; stream::SCRATCH_BYTES], |_| {
            Ok::<_, ()>(b"old".as_slice())
        })
        .unwrap();
        let mut calls = 0;
        let failure = read_text(
            fresh(&with_signatures()),
            &mut vec![0; stream::SCRATCH_BYTES],
            |_| {
                calls += 1;
                if calls == 3 {
                    Err("private failure /secret/path")
                } else {
                    Ok(b"signature".as_slice())
                }
            },
        )
        .unwrap_err();
        assert_eq!(calls, 3);
        assert_eq!(failure.target(), Some(inputs::Target::RelayPassword));
        assert_eq!(
            format!("{:?} {}", failure.error(), failure.error()),
            "config_materialize_input config_materialize_input"
        );
        assert!(std::error::Error::source(failure.error()).is_none());
        assert_eq!(old.relay_password().unwrap(), "old");
        let (storage, error) = failure.into_parts();
        assert!(matches!(
            error,
            Error::Input("private failure /secret/path")
        ));
        let next = read_text(
            loaded(BASE, storage),
            &mut vec![0; stream::SCRATCH_BYTES],
            |_| Ok::<_, ()>(b"new".as_slice()),
        )
        .unwrap();
        assert_eq!(next.relay_password().unwrap(), "new");
        assert_eq!(next.signatures(0).unwrap().unwrap().text, "");
    }
    #[test]
    fn short_scratch_refuses_before_open_and_returns_reusable_storage() {
        let _lock = lock();
        let mut calls = 0;
        let mut scratch = vec![0x55; material::SIGNATURE_SCRATCH_BYTES - 1];
        let failure = read_text(fresh(BASE), &mut scratch, |_| {
            calls += 1;
            Ok::<_, ()>(b"password".as_slice())
        })
        .unwrap_err();
        assert!(matches!(failure.error(), Error::Scratch));
        assert_eq!(failure.target(), None);
        assert_eq!(calls, 0);
        assert!(scratch.iter().all(|&byte| byte == 0x55));
        let (storage, _) = failure.into_parts();
        assert_eq!(
            loaded(BASE, storage)
                .candidate()
                .identities()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn aggregate_text_overflow_refuses_individually_valid_inputs() {
        let _lock = lock();
        let mut source = with_signatures();
        for n in 1..12u128 {
            source.push_str(&format!(
                r#"[identity "{n:032x}"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
text_signature_file = "/text"
html_signature_file = "/html"
"#
            ));
        }
        let bytes = vec![b'x'; material::MAX_SIGNATURE_BYTES];
        let mut calls = 0;
        let failed = read_text(fresh(&source), &mut vec![0; stream::SCRATCH_BYTES], |_| {
            calls += 1;
            Ok::<_, ()>(bytes.as_slice())
        })
        .unwrap_err();
        assert!(matches!(failed.error(), Error::Arena(text::Code::Capacity)));
        assert_eq!(calls, 12);
        let (storage, _) = failed.into_parts();
        assert_eq!(
            loaded(BASE, storage)
                .candidate()
                .identities()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn late_content_refusal_cannot_create_resolved_output() {
        let _lock = lock();
        let mut calls = 0;
        let failed = read_text(
            fresh(&with_signatures()),
            &mut vec![0; stream::SCRATCH_BYTES],
            |_| {
                calls += 1;
                Ok::<_, ()>(if calls == 2 {
                    b"\xff".as_slice()
                } else {
                    b"valid".as_slice()
                })
            },
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(
            failed.target(),
            Some(inputs::Target::HtmlSignature(
                crate::ids::IdentityId::from_bytes([0xbb; 16])
            ))
        );
        assert!(matches!(
            failed.error(),
            Error::Content(material::Error::Utf8)
        ));
        let (storage, _) = failed.into_parts();
        assert_eq!(
            loaded(BASE, storage)
                .candidate()
                .identities()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn late_read_failure_drops_reader_and_redacts_its_error() {
        let _lock = lock();
        struct LateRead<'a> {
            first: bool,
            closed: &'a std::cell::Cell<u32>,
        }
        impl io::Read for LateRead<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    output[..4].copy_from_slice(b"pass");
                    Ok(4)
                } else {
                    Err(io::Error::other("secret raw read error"))
                }
            }
        }
        impl Drop for LateRead<'_> {
            fn drop(&mut self) {
                self.closed.set(self.closed.get() + 1);
            }
        }
        let closed = std::cell::Cell::new(0);
        let failed = read_text(fresh(BASE), &mut vec![0; stream::SCRATCH_BYTES], |_| {
            Ok::<_, ()>(LateRead {
                first: true,
                closed: &closed,
            })
        })
        .unwrap_err();
        assert_eq!(closed.get(), 1);
        assert!(matches!(
            failed.error(),
            Error::Content(material::Error::Read(io::ErrorKind::Other))
        ));
        assert_eq!(
            format!("{:?} {}", failed.error(), failed.error()),
            "config_materialize_content config_materialize_content"
        );
        assert!(std::error::Error::source(failed.error()).is_none());
    }
    #[test]
    fn signature_completion_refuses_duplicate_stores() {
        let _lock = lock();
        let mut candidate = fresh(&with_signatures()).into_candidate();
        let id = crate::ids::IdentityId::from_bytes([0xbb; 16]);
        assert_eq!(candidate.signatures_complete(), Err(text::Code::Reference));
        let empty = candidate.append_material("").unwrap();
        candidate.store_signature(id, false, empty).unwrap();
        assert_eq!(candidate.signatures_complete(), Err(text::Code::Reference));
        assert_eq!(
            candidate.store_signature(id, false, empty),
            Err(text::Code::Reference)
        );
        candidate.store_signature(id, true, empty).unwrap();
        assert_eq!(candidate.signatures_complete(), Ok(()));
        assert_eq!(
            candidate.store_signature(id, true, empty),
            Err(text::Code::Reference)
        );
        let plain = fresh(BASE).into_candidate();
        assert_eq!(plain.signatures_complete(), Ok(()));
    }
    #[test]
    fn decoder_window_is_cleared_on_success_error_and_caught_unwind() {
        let _lock = lock();
        let mut scratch = vec![0x55; stream::SCRATCH_BYTES];
        for bytes in [b"password".as_slice(), b"bad\npassword".as_slice()] {
            scratch.fill(0x55);
            let result = read_text(fresh(BASE), &mut scratch, |_| Ok::<_, ()>(bytes));
            assert_eq!(result.is_ok(), bytes == b"password");
            assert!(scratch[..material::SIGNATURE_SCRATCH_BYTES]
                .iter()
                .all(|&byte| byte == 0));
            assert!(scratch[material::SIGNATURE_SCRATCH_BYTES..]
                .iter()
                .all(|&byte| byte == 0x55));
        }
        struct PanicRead;
        impl io::Read for PanicRead {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                output[..6].copy_from_slice(b"secret");
                panic!("fixture reader unwind");
            }
        }
        scratch.fill(0x55);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = read_text(fresh(BASE), &mut scratch, |_| Ok::<_, ()>(PanicRead));
        }));
        assert!(result.is_err());
        assert!(scratch[..material::SIGNATURE_SCRATCH_BYTES]
            .iter()
            .all(|&byte| byte == 0));
        assert!(scratch[material::SIGNATURE_SCRATCH_BYTES..]
            .iter()
            .all(|&byte| byte == 0x55));
    }
}
