//! Whole-reader structural loading. No protected-file or runtime authority.
use super::{dispatch, stanza::Pending, storage, stream};
use std::{fmt, io};

/// Reusable storage plus the precise stream or finalization refusal.
pub type Failure = storage::Failed<stream::Error<dispatch::Error>>;

/// Structural closure following EOF from this same reader operation.
/// The trusted Read implementation remains responsible for truthful EOF.
/// A statement-only candidate cannot be wrapped by callers:
/// ```compile_fail,E0451
/// use td_mta::config::{load::Loaded, storage::Candidate};
/// fn forge(candidate: Candidate) -> Loaded { Loaded { candidate } }
/// ```
pub struct Loaded {
    candidate: storage::Candidate,
}
impl fmt::Debug for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoadedConfiguration(<redacted>)")
    }
}
impl Loaded {
    pub fn candidate(&self) -> &storage::Candidate {
        &self.candidate
    }
    /// Retains structural validation but erases the whole-reader wrapper.
    pub fn into_candidate(self) -> storage::Candidate {
        self.candidate
    }
    pub fn into_storage(self) -> storage::Storage {
        self.candidate.into_storage()
    }
}

/// Drives the stream through actual EOF, then closes schema and references.
/// All buffers belong to the caller and are reused without resizing. Reader
/// allocation/blocking behavior and filesystem trust are adapter obligations.
// Inline failure retains the existing owner without an error-path allocation.
#[allow(clippy::result_large_err)]
pub fn read<R: io::Read + ?Sized>(
    storage: storage::Storage,
    pending: &mut Pending,
    scratch: &mut [u8],
    reader: &mut R,
) -> Result<Loaded, Failure> {
    storage
        .build_stanzas(pending, |mut sink| {
            stream::read(reader, scratch, |statement| sink.accept(statement)).map(|_| ())
        })
        .map(|candidate| Loaded { candidate })
}

// Named state only; tests/config_stack.rs separately qualifies the portable
// structural-loader stack. Future adapters/providers must qualify their frames.
const WORKSPACE_STATE_BYTES: usize = std::mem::size_of::<Pending>()
    + std::mem::size_of::<dispatch::Builder<'static, 'static>>()
    + std::mem::size_of::<dispatch::Parsed<'static>>()
    + std::mem::size_of::<Loaded>()
    + std::mem::size_of::<Failure>()
    + std::mem::size_of::<stream::Summary>()
    + std::mem::size_of::<super::syntax::Framer<'static>>()
    + std::mem::size_of::<super::syntax::Statement<'static>>();
const _: [(); 1] = [(); (WORKSPACE_STATE_BYTES <= 36 * 1024) as usize];
const _: [(); 1] = [(); (stream::SCRATCH_BYTES + 36 * 1024 <= 64 * 1024) as usize];

// The later finalizer borrows these views outside the loader workspace.
const BORROWED_VIEW_BYTES: usize = super::identity::MAX_IDENTITIES
    * std::mem::size_of::<super::identity::Identity<'static>>()
    + 2 * super::identity::MAX_IDENTITIES
        * super::identity::MAX_ADDRESSES
        * std::mem::size_of::<super::identity::Address<'static>>();
const _: [(); 1] = [(); (BORROWED_VIEW_BYTES <= 80 * 1024) as usize];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::super::{graph, identities, policy, routing, text};
    use super::*;
    use storage::{BuildError, Storage};
    const SOURCE: &str = r#"version = 1
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
            .unwrap_or_else(|e| e.into_inner())
    }
    struct Reader<'a> {
        bytes: &'a [u8],
        chunk: usize,
        terminal: Option<io::ErrorKind>,
        ends: usize,
    }
    impl io::Read for Reader<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.bytes.is_empty() {
                self.ends += 1;
                return self.terminal.map_or(Ok(0), |kind| Err(kind.into()));
            }
            let n = self.bytes.len().min(self.chunk).min(out.len());
            out[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }
    fn reader(bytes: &[u8], chunk: usize) -> Reader<'_> {
        Reader {
            bytes,
            chunk,
            terminal: None,
            ends: 0,
        }
    }
    #[test]
    fn complete_source_requires_eof_at_every_chunk_and_final_line_shape() {
        let _lock = lock();
        let mut storage = Storage::try_new().unwrap();
        let allocated = storage.allocated_bytes().unwrap();
        let mut pending = Pending::new();
        let mut scratch = vec![0xaa; stream::SCRATCH_BYTES + 16];
        for source in [SOURCE, SOURCE.trim_end()] {
            for chunk in [1, 2, 7, 31, stream::INPUT_BYTES] {
                let mut input = reader(source.as_bytes(), chunk);
                let loaded = read(storage, &mut pending, &mut scratch, &mut input).unwrap();
                assert_eq!(input.ends, 1);
                let c = loaded.candidate();
                assert_eq!(c.identities().unwrap().len(), 1);
                assert_eq!(
                    c.outbound().unwrap().relay().unwrap().username,
                    "private-relay"
                );
                c.with_graph(|records, text| {
                    let view = records.view(text).unwrap();
                    assert_eq!(view.binding_count(), 2);
                })
                .unwrap();
                assert_eq!(&scratch[stream::SCRATCH_BYTES..], &[0xaa; 16]);
                assert_eq!(format!("{loaded:?}"), "LoadedConfiguration(<redacted>)");
                storage = if chunk == 1 {
                    let candidate = loaded.into_candidate();
                    assert_eq!(candidate.identities().unwrap().len(), 1);
                    candidate.into_storage()
                } else {
                    loaded.into_storage()
                };
                assert_eq!(storage.allocated_bytes().unwrap(), allocated);
            }
        }
    }
    #[test]
    fn late_read_failure_returns_storage_and_leaves_prior_candidate_readable() {
        let _lock = lock();
        let mut pending = Pending::new();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let old = read(
            Storage::try_new().unwrap(),
            &mut pending,
            &mut scratch,
            &mut SOURCE.as_bytes(),
        )
        .unwrap();
        let mut storage = Storage::try_new().unwrap();
        let bytes = storage.allocated_bytes().unwrap();
        for end in [1, 50, SOURCE.len()] {
            let mut input = reader(&SOURCE.as_bytes()[..end], 13);
            input.terminal = Some(io::ErrorKind::PermissionDenied);
            let failed = read(storage, &mut pending, &mut scratch, &mut input).unwrap_err();
            assert_eq!(input.ends, 1);
            assert!(matches!(
                failed.error(),
                BuildError::Input(stream::Error::Read(io::ErrorKind::PermissionDenied))
            ));
            (storage, _) = failed.into_parts();
            assert_eq!(storage.allocated_bytes().unwrap(), bytes);
            let new = read(storage, &mut pending, &mut scratch, &mut SOURCE.as_bytes()).unwrap();
            assert_eq!(
                old.candidate()
                    .outbound()
                    .unwrap()
                    .relay()
                    .unwrap()
                    .username,
                "private-relay"
            );
            assert_eq!(new.candidate().identities().unwrap().len(), 1);
            storage = new.into_storage();
        }
    }
    #[test]
    fn syntax_handler_and_finalizer_refusals_are_typed_and_redacted() {
        let _lock = lock();
        let mut storage = Storage::try_new().unwrap();
        let mut pending = Pending::new();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let syntax = format!("{SOURCE}private-invalid-key = \"unterminated");
        let handler = format!("{SOURCE}unknown_private_key = \"private-value\"\n");
        let finalizer = SOURCE.replace(
            "certificate = \"public\"",
            "certificate = \"missing-private\"",
        );
        for (index, source) in [syntax, handler, finalizer, String::new()]
            .iter()
            .enumerate()
        {
            let mut input = reader(source.as_bytes(), 13);
            let failed = read(storage, &mut pending, &mut scratch, &mut input).unwrap_err();
            match (index, failed.error()) {
                (0, BuildError::Input(stream::Error::Syntax(_))) => (),
                (1, BuildError::Configuration(_)) => (),
                (2, BuildError::Configuration(error)) => {
                    assert!(
                        matches!(error.cause, dispatch::Cause::Graph(g) if g.code == graph::Code::UnknownCertificate)
                    );
                    assert!(error.context.is_none());
                    assert_eq!(input.ends, 1);
                }
                (3, BuildError::Configuration(error)) => assert!(matches!(error.cause,
                    dispatch::Cause::Dispatch(d) if d.code == dispatch::Code::MissingVersion)),
                _ => panic!("wrong refusal"),
            }
            let printed = format!("{failed:?} {:?} {}", failed.error(), failed.error());
            assert!(!printed.contains("private"));
            assert!(std::error::Error::source(failed.error()).is_none());
            (storage, _) = failed.into_parts();
        }
        assert!(read(storage, &mut pending, &mut scratch, &mut SOURCE.as_bytes()).is_ok());
    }
    #[test]
    fn short_scratch_refuses_before_reader_io_and_can_be_retried() {
        let _lock = lock();
        let mut input = reader(SOURCE.as_bytes(), 1);
        let mut pending = Pending::new();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let failed = read(
            Storage::try_new().unwrap(),
            &mut pending,
            &mut scratch[..stream::SCRATCH_BYTES - 1],
            &mut input,
        )
        .unwrap_err();
        assert!(matches!(
            failed.error(),
            BuildError::Input(stream::Error::Capacity)
        ));
        assert_eq!(input.bytes, SOURCE.as_bytes());
        assert_eq!(input.ends, 0);
        let (storage, _) = failed.into_parts();
        assert!(read(storage, &mut pending, &mut scratch, &mut input).is_ok());
    }
    #[test]
    fn descriptor_and_shared_text_exhaustion_return_reusable_storage() {
        let _lock = lock();
        let mut storage = Storage::try_new().unwrap();
        let mut pending = Pending::new();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let mut domains = SOURCE.to_owned();
        for n in 0..routing::MAX_DOMAINS {
            domains.push_str(&format!("[domain \"d{n}.test\"]\n"));
        }
        let mut names = SOURCE.to_owned();
        let name = "x".repeat(4096);
        for n in 1..64 {
            names.push_str(&format!("[identity \"{n:032x}\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nemail = \"main@example.test\"\nname = \"{name}\"\n"));
        }
        for (index, source) in [&domains, &names].iter().enumerate() {
            let failed =
                read(storage, &mut pending, &mut scratch, &mut source.as_bytes()).unwrap_err();
            let error = match failed.error() {
                BuildError::Configuration(e) => e,
                _ => panic!("not a schema capacity error"),
            };
            assert!(error.context.is_some());
            match (index, error.cause) {
                (0, dispatch::Cause::Policy(e)) => {
                    assert_eq!(e.code, policy::Code::Routing);
                    assert_eq!(e.routing, Some(routing::Code::Capacity));
                }
                (1, dispatch::Cause::Identities(e)) => assert_eq!(e.code, identities::Code::Text),
                _ => panic!("unexpected capacity origin"),
            }
            (storage, _) = failed.into_parts();
            storage = read(storage, &mut pending, &mut scratch, &mut SOURCE.as_bytes())
                .unwrap()
                .into_storage();
        }
    }
}
