#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Source-level contracts the compiler cannot express: the crate's file
//! inventory, and which toolkit modules its files may name. td-ui/DESIGN.md
//! requires each consumer to pin the second in its own confinement tests.
//! `window` is the crate's only Wayland client: it alone names
//! `td_ui::wayland` and `td_ui::client`, so the pages stay pure rendering
//! over the raster and the chrome bands and cannot quietly reach the
//! transport or the turn loop. `service` is the crate's only installer
//! client: it alone names the protocol, the setup intake and threads.
//! `recovery` alone names the disk protector, only its recovery-key codec.

use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn source_inventory_and_toolkit_access_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists(), "no build script");
    // The crate root forbids unsafe for the whole crate.
    assert!(
        std::fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("#![forbid(unsafe_code)]"),
        "crate root forbids unsafe"
    );
    let expected: BTreeSet<String> = [
        "destination.rs",
        "evidence.rs",
        "lib.rs",
        "main.rs",
        "outcome.rs",
        "recovery.rs",
        "review.rs",
        "service.rs",
        "settings.rs",
        "welcome.rs",
        "window.rs",
    ]
    .iter()
    .map(|name| name.to_string())
    .collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file(), "no nested source");
        let name = entry.file_name().into_string().unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        // Only `window` reaches the live client, reads the outline face
        // and keeps the theme; every other file, the pages included,
        // stays off the transport, the turn loop and the filesystem.
        if name != "window.rs" {
            for module in [
                "td_ui::wayland",
                "td_ui::client",
                "td_ui::pinned_face",
                "td_ui::theme_file",
            ] {
                assert!(
                    !text.contains(module),
                    "{name} names {module}; only window may"
                );
            }
        }
        actual.insert(name);
    }
    assert_eq!(actual, expected, "unexpected or missing source file");
    let installer = std::fs::read_to_string(root.join("../td-install/src/lib.rs")).unwrap();
    let active: Vec<_> = installer
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect();
    assert_eq!(
        active,
        [
            "#![forbid(unsafe_code)]",
            "pub mod installation_plan;",
            "pub mod installation_protocol;"
        ],
        "installer library source or API grew"
    );
    for name in [
        "destination.rs",
        "evidence.rs",
        "lib.rs",
        "main.rs",
        "outcome.rs",
        "recovery.rs",
        "review.rs",
        "service.rs",
        "settings.rs",
        "welcome.rs",
        "window.rs",
    ] {
        let text = std::fs::read_to_string(root.join("src").join(name)).unwrap();
        let allowed = if matches!(name, "destination.rs" | "review.rs" | "window.rs") {
            text.replace("td_install::installation_plan::", "")
        } else if name == "service.rs" {
            text.replace("td_install::installation_plan::", "")
                .replace("td_install::installation_protocol::", "")
        } else if name == "lib.rs" {
            text.replacen(
                "use td_install::installation_plan::{\n    Basis, Destination, DestinationObservation, Plan, Settings, Storage,\n};",
                "",
                1,
            )
        } else {
            text.clone()
        };
        assert!(
            !allowed.contains("td_install"),
            "{name} reaches another installer API"
        );
        // Only `recovery` names the disk protector, and only its
        // recovery-key codec: no TPM client, token, header reader or key
        // generation reaches the window.
        let protector = if name == "recovery.rs" {
            text.replace("td_protector::recovery::", "")
        } else {
            text.clone()
        };
        assert!(
            !protector.contains("td_protector"),
            "{name} reaches the disk protector"
        );
        let found = client_violations(name, &text);
        assert!(found.is_empty(), "{name}: {found:?}");
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !compact.contains("path=") && !compact.contains("include!("),
            "{name} mounts outside source"
        );
    }
    let recovery = std::fs::read_to_string(root.join("src/recovery.rs")).unwrap();
    assert!(recovery.contains("use td_protector::recovery::{"));
    assert!(!recovery.contains("generate"), "td-setup draws no key");
    // The client boundary is real: `window` names both the transport and the
    // client, so removing either from it would be caught here, not silently.
    let window = std::fs::read_to_string(root.join("src/window.rs")).unwrap();
    assert!(
        window.contains("td_ui::wayland") && window.contains("td_ui::client"),
        "window is the crate's Wayland client boundary"
    );
    // The installer client reaches exactly td-authd's setup intake, and
    // the window connects only through it.
    let service = std::fs::read_to_string(root.join("src/service.rs")).unwrap();
    let service = production(&service).unwrap();
    assert_eq!(service.matches("/run/").count(), 1);
    assert!(service.contains("pub const SOCKET: &str = \"/run/td-authd/1000/setup\";"));
    assert_eq!(service.matches("connect(").count(), 2);
    assert_eq!(service.matches("UnixStream::connect(").count(), 1);
    assert!(window.contains("self.front.key(&chord, Path::new(SOCKET));"));
    // td-ui ticks only before an event; every turn, idle ones too, ends in
    // `end_turn`, so the answer is taken there.
    let end_turn: String = window
        .split("fn end_turn(")
        .nth(1)
        .unwrap()
        .chars()
        .take(120)
        .collect();
    assert!(end_turn.contains("self.receive();"), "{end_turn}");
}

/// A file's production source: everything before its trailing test
/// module, which is `mod tests` under `#[cfg(test)]` and must end the file.
/// A file without one is all production, lone test items included.
fn production(text: &str) -> Result<&str, String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(header) = lines
        .iter()
        .position(|line| *line == "mod tests {" || *line == "pub(crate) mod tests {")
    else {
        return Ok(text);
    };
    let mut first = header;
    while first > 0 && lines[first - 1].starts_with("#[") {
        first -= 1;
    }
    if !lines[first..header].contains(&"#[cfg(test)]") {
        return Err("a tests module outside #[cfg(test)]".into());
    }
    // Inside the module rustfmt indents everything; only its close sits at
    // the margin, and nothing follows it.
    let rest: Vec<&str> = lines[header + 1..]
        .iter()
        .copied()
        .filter(|line| !line.is_empty() && !line.starts_with(' '))
        .collect();
    if rest != ["}"] {
        return Err(format!("code after the tests module: {rest:?}"));
    }
    let offset: usize = lines[..first].iter().map(|line| line.len() + 1).sum();
    Ok(text.get(..offset).unwrap_or(text))
}

/// Spellings by which a file other than `service` could reach the
/// installer service, a socket or a thread, however it imports them: the
/// bare names are refused, not only their usual paths. `window` keeps its
/// Wayland stream and connects only as the toolkit and `service` do.
fn client_violations(name: &str, text: &str) -> Vec<String> {
    let mut code = match production(text) {
        Ok(code) => code.to_string(),
        Err(why) => return vec![why],
    };
    let forbidden: &[&str] = if name == "service.rs" {
        // No re-export, alias or exported macro, at any visibility, hands
        // another file a name.
        if code.contains("macro_export")
            || code.lines().any(|line| {
                line.trim_start().starts_with("pub")
                    && (line.contains(" use ") || line.contains(" type "))
            })
        {
            return vec!["re-exports".into()];
        }
        &["pub use", "UnixListener", "UnixDatagram", "std::process"]
    } else {
        if name == "window.rs" {
            for allowed in [
                "use std::os::unix::net::UnixStream;",
                "fn new(stream: UnixStream,",
                "let stream = connect(endpoint)?;",
                "use td_ui::wayland::{connect, endpoint};",
                "Service::connect(intake)",
                // The toolkit's keyboard state, not `std::sync`.
                "&& input.synchronized &&",
            ] {
                code = code.replacen(allowed, "", 1);
            }
        }
        &[
            "installation_protocol",
            "/run/",
            "thread",
            "mpsc",
            "sync",
            "unix::net",
            "UnixStream",
            "UnixListener",
            "UnixDatagram",
            "std::net",
            "connect(",
        ]
    };
    forbidden
        .iter()
        .filter(|token| code.contains(**token))
        .map(|token| format!("names {token}"))
        .collect()
}

#[test]
fn the_client_guard_refuses_grouped_imports_aliases_and_reexports() {
    for (name, text) in [
        ("welcome.rs", "use std::{sync::mpsc, thread};"),
        ("destination.rs", "use std::os::unix::net::UnixStream as S;"),
        ("review.rs", "fn f(p: &str) { let _ = S::connect(p); }"),
        (
            "window.rs",
            "use std::{os::unix::net::UnixStream as Other};",
        ),
        ("window.rs", "let other = connect(somewhere)?;"),
        (
            "outcome.rs",
            "const P: &str = \"/run/td-authd/1000/setup\";",
        ),
        (
            "service.rs",
            "pub use td_install::installation_protocol::Request;",
        ),
        ("service.rs", "pub(crate) use std::thread as worker;"),
        (
            "service.rs",
            "pub(crate) type Builder = std::thread::Builder;",
        ),
        (
            "service.rs",
            "#[macro_export]\nmacro_rules! m { () => {}; }",
        ),
        // A lone test item does not end production.
        (
            "welcome.rs",
            "#[cfg(test)]\n#[test]\nfn t() {}\nfn f() { std::thread::spawn(|| {}); }\n",
        ),
        // Nor does a module that production follows.
        (
            "welcome.rs",
            "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn f() { std::thread::spawn(|| {}); }\n",
        ),
    ] {
        assert!(!client_violations(name, text).is_empty(), "{name}: {text}");
    }
    // A trailing test module is not production.
    assert!(client_violations(
        "welcome.rs",
        "fn f() {}\n\n#[cfg(test)]\n#[allow(clippy::unwrap_used)]\nmod tests {\n    use std::thread;\n}\n"
    )
    .is_empty());
    assert!(client_violations("window.rs", "use std::os::unix::net::UnixStream;").is_empty());
}

/// The boot evidence reads one file, the kernel's command line, and only
/// `evidence` opens a file or names `/proc`; the window reaches it only
/// through that module's own path.
#[test]
fn only_the_evidence_reads_the_command_line() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let code = production(&text).unwrap();
        if name == "evidence.rs" {
            assert_eq!(code.matches("/proc/").count(), 1, "{name}");
            assert!(code.contains("pub const CMDLINE: &str = \"/proc/cmdline\";"));
            assert_eq!(code.matches("File::open(").count(), 1, "{name}");
            assert!(!code.contains("std::io::stderr"), "{name} writes");
        } else {
            for token in ["/proc/", "File::open(", "std::fs::File", "use std::fs"] {
                assert!(!code.contains(token), "{name} names {token}");
            }
        }
    }
    let window = std::fs::read_to_string(root.join("src/window.rs")).unwrap();
    assert!(window.contains("evidence::enabled(Path::new(evidence::CMDLINE))"));
}

/// The inventory above walks `src`; a test file could still mount a sibling
/// crate's source by `#[path]` and so smuggle a second copy of, say, a
/// toolkit module. The test tree is held to the same line: every `#[path]`
/// mount stays inside this crate's `tests`, so the native harness under
/// `tests/support` cannot reach up into td-ui or td-compositor. Fixtures read
/// by `include_str!` are data, not modules, and are not constrained here.
#[test]
fn test_files_mount_no_sibling_source() {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &mut files,
    );
    assert!(files.len() >= 3, "{files:?}");
    let mut mounted = false;
    for file in files {
        // The guard names these attribute spellings in its own logic and
        // comments; it declares no modules, so skip it rather than self-match.
        if file.file_name().and_then(|name| name.to_str()) == Some("confinement.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        // A raw-string path attribute (`path=r"..."` or `path=r#"..."#`, direct
        // or via `cfg_attr`) would carry a `..` past the value scan below; the
        // crate has no use for one, so forbid the spelling outright.
        assert!(
            !compact.contains("path=r"),
            "raw-string #[path] mount in {}",
            file.display()
        );
        // Every plain path attribute value, `#[path="..."]` or a conditional
        // `#[cfg_attr(_, path="...")]`, wherever `..` sits in it:
        // `support/../../x` reaches as far as `../../x`.
        for mount in compact.split("path=\"").skip(1) {
            mounted = true;
            let value = mount.split('"').next().unwrap_or(mount);
            assert!(
                !value.contains("..") && !value.starts_with('/'),
                "source mounted from outside the test tree: {} ({value})",
                file.display()
            );
        }
    }
    // The native harness is mounted by path; if that mount ever disappears the
    // check above would pass vacuously, so require at least one to exist.
    assert!(mounted, "no #[path] mount found in the test tree");
}
