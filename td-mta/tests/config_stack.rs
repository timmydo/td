//! Exercises loader fixtures on the host and qualifies their portable compilation.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::io;
use td_mta::config::storage::{BuildError, Storage};
use td_mta::config::{dispatch, load::read, stanza::Pending, stream};
use td_mta::config::{graph, identities, policy, routing};
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
fn complete_source_requires_eof_at_every_chunk_and_final_line_shape() {
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
fn late_read_failure_returns_storage_and_leaves_prior_candidate_readable() {
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
fn syntax_handler_and_finalizer_refusals_are_typed_and_redacted() {
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
fn short_scratch_refuses_before_reader_io_and_can_be_retried() {
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
fn descriptor_and_shared_text_exhaustion_return_reusable_storage() {
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
        names.push_str(&format!(
            r#"[identity "{n:032x}"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
name = "{name}"
"#
        ));
    }
    for (index, source) in [&domains, &names].iter().enumerate() {
        let failed = read(storage, &mut pending, &mut scratch, &mut source.as_bytes()).unwrap_err();
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
fn largest_pending_source() {
    let domain = format!("{}.{}.{}", "a".repeat(63), "b".repeat(63), "c".repeat(61));
    let local = "x".repeat(64);
    let path = format!("/{}", "x".repeat(4094));
    let runtime = format!("/r{}", "x".repeat(4093));
    let logs = format!("/l{}", "x".repeat(4093));
    let source = SOURCE
        .replace("example.test", &domain)
        .replace("main@", &format!("{local}@"));
    let source = source.replace(
        "[resolver \"primary\"]",
        &format!(
            r#"name = "{}"
text_signature_file = "{path}"
html_signature_file = "{path}"
[paths]
data = "{path}"
runtime = "{runtime}"
logs = "{logs}"
[resolver "primary"]"#,
            "x".repeat(4096)
        ),
    );
    let mut storage = Storage::try_new().unwrap();
    let mut pending = Pending::new();
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    for chunk in [1, stream::INPUT_BYTES] {
        let mut input = reader(source.as_bytes(), chunk);
        let loaded = read(storage, &mut pending, &mut scratch, &mut input)
            .unwrap_or_else(|failed| panic!("{} {:?}", failed.error(), failed.error()));
        assert_eq!(input.ends, 1);
        assert_eq!(loaded.candidate().identities().unwrap().len(), 1);
        storage = loaded.into_storage();
    }
}
fn full_routing_source() {
    let mut source = SOURCE.to_owned();
    assert!(routing::MAX_ALIASES.is_power_of_two());
    for n in (1..routing::MAX_ALIASES).map(|n| n * 2053 % routing::MAX_ALIASES) {
        source.push_str(&format!(
            "[alias \"u{n:04}@example.test\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n"
        ));
    }
    for n in (1..routing::MAX_DOMAINS).rev() {
        source.push_str(&format!("[domain \"d{n:03}.test\"]\n"));
    }
    let mut storage = Storage::try_new().unwrap();
    let mut pending = Pending::new();
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    let loaded = read(
        storage,
        &mut pending,
        &mut scratch,
        &mut reader(source.as_bytes(), 127),
    )
    .unwrap_or_else(|failed| panic!("{} {:?}", failed.error(), failed.error()));
    loaded
        .candidate()
        .with_graph(|records, text| {
            let routes = records.routes();
            assert!(routes.resolve("u4095@example.test").unwrap().is_some());
            assert!(records.view(text).is_ok());
        })
        .unwrap();
    storage = loaded.into_storage();
    assert!(read(storage, &mut pending, &mut scratch, &mut SOURCE.as_bytes()).is_ok());
}
fn profile_variants() {
    let gateway = SOURCE
        .replace(
            "kind = \"direct_smtp\"",
            "kind = \"gateway_smtp\"\ngateway = \"trusted\"",
        )
        .replace(
            "[domain \"example.test\"]",
            r#"[domain "example.test"]
mx_host = "upstream.example.test"
mta_sts = "testing"
mta_sts_certificate = "public""#,
        )
        + &format!(
            r#"[gateway_peer "trusted"]
network = "192.0.2.0/24"
[gateway "trusted"]
ca_file = "/gateway-ca"
client_cert_sha256 = "{}"
next_client_cert_sha256 = "{}"
[limits]
smtp_sessions = 4
[disk]
[work]
[network]
"#,
            "a".repeat(64),
            "b".repeat(64)
        );
    let acme = SOURCE.replace(
        "mode = \"files\"\nchain_file = \"/private-chain\"\nkey_file = \"/private-key\"",
        "mode = \"acme\"",
    ) + r#"[acme]
directory = "https://ca.example.test/directory"
contact = "main@example.test"
terms_accepted = true
ca_file = "/ca"
[listener "challenge"]
kind = "http01"
bind = "0.0.0.0:80"
"#;
    let loopback = SOURCE.replace(
        "[resolver \"primary\"]",
        "reply_to = true\n[resolver \"primary\"]",
    ) + r#"[listener "fixture"]
kind = "loopback_smtp_fixture"
bind = "127.0.0.1:2525"
session_limit = 1
per_peer_limit = 1
[logging]
minimum_severity = "info"
[identity_address "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
kind = "reply_to"
name = "Reply"
email = "main@example.test"
"#;
    let mut storage = Storage::try_new().unwrap();
    let mut pending = Pending::new();
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    for source in [gateway, acme, loopback] {
        let loaded = read(
            storage,
            &mut pending,
            &mut scratch,
            &mut reader(source.as_bytes(), 17),
        )
        .unwrap_or_else(|failed| panic!("{} {:?}", failed.error(), failed.error()));
        assert_eq!(loaded.candidate().identities().unwrap().len(), 1);
        storage = loaded.into_storage();
    }
}

fn bounded_stack_mapping() -> usize {
    use std::io::Read;
    let marker = 0u8;
    let address = std::hint::black_box(&marker) as *const u8 as usize;
    let mut text = String::new();
    std::fs::File::open("/proc/self/smaps")
        .unwrap()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.len() <= 1024 * 1024);
    let size = stack_mapping(&text, address).unwrap();
    println!("config_stack_mapping_bytes={size}");
    size
}

fn stack_mapping(text: &str, address: usize) -> Option<usize> {
    let mut previous: Option<(usize, usize, &str)> = None;
    let mut selected = None;
    for line in text.lines() {
        if let Some(flags) = line.strip_prefix("VmFlags:") {
            if let Some(size) = selected {
                return (!flags.split_whitespace().any(|flag| flag == "gd")).then_some(size);
            }
        }
        let mut fields = line.split_whitespace();
        let Some((low, high)) = fields.next().and_then(|word| word.split_once('-')) else {
            continue;
        };
        if selected.is_some() {
            return None;
        }
        let low = usize::from_str_radix(low, 16).ok()?;
        let high = usize::from_str_radix(high, 16).ok()?;
        let size = high.checked_sub(low)?;
        let perms = fields.next()?;
        if (low..high).contains(&address) {
            let (guard_low, guard_high, guard_perms) = previous?;
            if perms != "rw-p"
                || guard_perms != "---p"
                || guard_high != low
                || guard_high.checked_sub(guard_low)? < 4096
                || size > 176 * 1024
            {
                return None;
            }
            selected = Some(size);
        }
        previous = Some((low, high, perms));
    }
    None
}

#[test]
fn mapping_evidence_refuses_unbounded_or_missing_guards() {
    let text = r#"1000-2000 ---p 00000000 00:00 0
VmFlags: mr mw me
2000-4000 rw-p 00000000 00:00 0
VmFlags: rd wr mr mw me
"#;
    assert_eq!(stack_mapping(text, 0x3000), Some(8192));
    for bad in [
        text.replace("2000-4000", "2000-30000"),
        text.replace("VmFlags: rd wr", "VmFlags: gd rd wr"),
        text.replace("---p", "rw-p"),
        text.replace("1000-2000", "1800-2000"),
        text.replace("1000-2000", "0000-1000"),
        text.replace("VmFlags: rd wr mr mw me\n", ""),
    ] {
        assert_eq!(stack_mapping(&bad, 0x3000), None);
    }
    assert_eq!(stack_mapping(text, 0x5000), None);
}

#[test]
#[ignore = "run by the isolated pinned-musl qualification"]
fn portable_loader_stack() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) || cfg!(debug_assertions)
    {
        return Err("requires the pinned release x86-64 musl artifact".into());
    }
    std::thread::Builder::new()
        .name("config-stack".into())
        .stack_size(160 * 1024)
        .spawn(|| {
            assert!(bounded_stack_mapping() <= 176 * 1024);
            loader_scenarios();
        })?
        .join()
        .map_err(|_| "configuration stack worker failed")?;
    Ok(())
}

fn loader_scenarios() {
    largest_pending_source();
    full_routing_source();
    profile_variants();
    complete_source_requires_eof_at_every_chunk_and_final_line_shape();
    late_read_failure_returns_storage_and_leaves_prior_candidate_readable();
    syntax_handler_and_finalizer_refusals_are_typed_and_redacted();
    short_scratch_refuses_before_reader_io_and_can_be_retried();
    descriptor_and_shared_text_exhaustion_return_reusable_storage();
}

#[test]
fn host_loader_scenarios() {
    // Checks fixture drift; the host harness stack is not target evidence.
    loader_scenarios();
}
