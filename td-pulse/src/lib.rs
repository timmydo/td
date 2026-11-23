//! The PulseAudio native protocol as td speaks it: the pstream framing
//! (`wire`), the tagstruct codec (`tag`), the captured command table, error
//! codes and version negotiation (`proto`), the blocking control client td's
//! controllers share, which also plays one stream for `td-audio play`
//! (`client`), td-audio's socket as its transport (`socket`), and what a
//! controller shows and groups of a stream listing (`apps`).
//!
//! The normative design is `APPLICATIONS.md` §K.3. `td-audio` serves this
//! protocol on its socket; a control client speaks the same bytes, so the
//! codec lives here once rather than in a copy per consumer. Every module is
//! safe code: the daemon's syscall surface stays in `td-audio`.
#![forbid(unsafe_code)]

pub mod apps;
pub mod client;
pub mod proto;
pub mod socket;
pub mod tag;
pub mod wire;

#[cfg(test)]
mod tests {
    /// Every source file of this crate, read at compile time.
    fn sources() -> Vec<(&'static str, &'static str)> {
        vec![
            ("apps.rs", include_str!("apps.rs")),
            ("client.rs", include_str!("client.rs")),
            ("lib.rs", include_str!("lib.rs")),
            ("proto.rs", include_str!("proto.rs")),
            ("socket.rs", include_str!("socket.rs")),
            ("tag.rs", include_str!("tag.rs")),
            ("wire.rs", include_str!("wire.rs")),
        ]
    }

    /// The module names a source text declares with `mod NAME;`, over the
    /// whole text so a declaration split across lines is still seen.
    fn declared_in(text: &str) -> Vec<String> {
        let code: String = text
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let mut found = Vec::new();
        let mut words = code.split_whitespace().peekable();
        while let Some(word) = words.next() {
            if word != "mod" {
                continue;
            }
            if let Some(name) = words.peek().and_then(|next| next.strip_suffix(';')) {
                found.push(name.to_string());
            }
        }
        found
    }

    /// The scanned list, the declared modules and the files on disk are one
    /// set, and the crate is flat: a nested module or an unlisted file would
    /// be compiled and read by no assertion here.
    #[test]
    fn the_scan_covers_every_file_and_module() {
        let mut declared: Vec<String> = declared_in(include_str!("lib.rs"))
            .into_iter()
            .map(|name| format!("{name}.rs"))
            .chain(std::iter::once("lib.rs".to_string()))
            .collect();
        let mut scanned: Vec<String> = sources().iter().map(|(f, _)| f.to_string()).collect();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut on_disk = Vec::new();
        for entry in dir.read_dir().unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(entry.file_type().unwrap().is_file(), "{name} is not a file");
            if name.ends_with(".rs") {
                on_disk.push(name);
            }
        }
        declared.sort();
        scanned.sort();
        on_disk.sort();
        assert_eq!(declared, scanned);
        assert_eq!(on_disk, scanned);
        for (file, text) in sources() {
            if file != "lib.rs" {
                assert!(declared_in(text).is_empty(), "{file} declares a submodule");
            }
        }
    }

    /// Line comments stripped and whitespace squeezed, so a match is immune to
    /// reformatting and a commented-out line counts for nothing.
    fn code(text: &str) -> String {
        text.lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .flat_map(str::chars)
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// The codec is safe code with no way to bring in an unscanned file: the
    /// keyword appears once, in the root's prohibition, which is code rather
    /// than a comment, and no source includes a file by path, redirects a
    /// module, writes an attribute conditionally or opens a block comment
    /// (which could split any of those from the scans).
    #[test]
    fn the_codec_is_safe_and_self_contained() {
        let keyword = concat!("un", "safe");
        for (file, text) in sources() {
            let expected = usize::from(file == "lib.rs");
            assert_eq!(text.matches(keyword).count(), expected, "{file}");
            let squeezed = code(text);
            for refused in [
                concat!("incl", "ude!"),
                concat!("#[pa", "th"),
                concat!("#[cfg_", "attr("),
                concat!("/", "*"),
            ] {
                assert!(!squeezed.contains(refused), "{file} contains {refused}");
            }
        }
        assert!(code(include_str!("lib.rs")).contains(concat!("#![forbid(", "un", "safe_code)]")));
    }

    /// The only lines that may name these modules, each in one file, exactly
    /// once and as a whole line, so a `pub` prefix re-exporting one fails:
    /// `socket.rs`'s connected stream, its path, its io traits and errors,
    /// its clock and its tests' threads, and this file's test reading its
    /// own source directory. The last field is whether the line must sit in
    /// the file's test module. Split so this list does not match itself.
    const PERMITTED: &[(&str, &str, bool)] = &[
        (
            "socket.rs",
            concat!("use std::", "io::{Error, ErrorKind, Read, Result, Write};"),
            false,
        ),
        (
            "socket.rs",
            concat!("use std::", "os::unix::net::UnixStream;"),
            false,
        ),
        ("socket.rs", concat!("use std::", "path::Path;"), false),
        (
            "socket.rs",
            concat!("use std::", "time::{Duration, Instant};"),
            false,
        ),
        ("socket.rs", concat!("use std::", "thread;"), true),
        (
            "lib.rs",
            concat!(
                "let dir = std::",
                "path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"src\");"
            ),
            true,
        ),
    ];

    /// Whether the squeezed text declares a public `item` (`use`, `type`),
    /// plainly or restricted to a path.
    fn public(squeezed: &str, item: &str) -> bool {
        let restricted = squeezed.match_indices("pub(").any(|(at, _)| {
            squeezed
                .get(at..)
                .and_then(|rest| rest.find(')').and_then(|close| rest.get(close + 1..)))
                .is_some_and(|after| after.starts_with(item))
        });
        restricted
            || squeezed.match_indices("pub").any(|(at, _)| {
                squeezed
                    .get(at + 3..)
                    .is_some_and(|after| after.starts_with(item))
            })
    }

    /// Where the file's test module starts, when it is `#[cfg(test)]` and
    /// the file's last item: after it, the only line that closes an item at
    /// the left margin is the file's last.
    fn test_module(text: &str) -> Option<usize> {
        let at = text.find(concat!("#[cfg(test)]\nmod ", "tests {\n"))?;
        let closing: Vec<usize> = text
            .get(at..)?
            .lines()
            .enumerate()
            .filter(|(_, line)| *line == "}")
            .map(|(number, _)| number)
            .collect();
        let last = text.get(at..)?.lines().count().checked_sub(1)?;
        (closing == [last]).then_some(at)
    }

    /// The codec is bytes in and bytes out. Outside the lines `PERMITTED`
    /// names, no file names an OS facility, a clock, a thread, a
    /// syscall, a device or the PCM and control requests td-audio refuses
    /// (`UNSAFE.md` §13): the daemon links this crate, so a name here would
    /// reach the shipped binary past td-audio's own scans. Whole files, not
    /// the text before the tests: production code may follow a test module.
    /// Comments are kept, so a `//` inside a string literal hides nothing and
    /// a mention fails closed; a grouped `std` import is refused because it
    /// names a module without any of the paths below. Each name is split so
    /// this list does not match itself.
    ///
    /// This is a tripwire against reaching an OS facility by accident, not a
    /// proof against code written to evade it, which a name scan cannot be:
    /// that is review's to catch. It closes the spellings review found.
    #[test]
    fn the_codec_reaches_no_device_or_operating_system_facility() {
        for (file, text) in sources() {
            let mut squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(!public(&squeezed, "use"), "{file} re-exports");
            // A path split across lines by a comment would squeeze to one
            // the list below never sees whole. Cut at the first `//`, and
            // whole, since a `//` in a string literal would cut too early.
            for line in text.lines() {
                for code in [line.split("//").next().unwrap_or(""), line] {
                    let code = code.trim();
                    assert!(
                        !code.starts_with("::") && !code.ends_with("::"),
                        "{file} splits a path: {line}"
                    );
                }
            }
            let tests_at = test_module(text);
            for (owner, line, test_only) in PERMITTED.iter().filter(|(owner, ..)| *owner == file) {
                let whole = text.lines().filter(|text| text.trim() == *line).count();
                if *test_only {
                    let at = text.find(line);
                    assert!(
                        tests_at.is_some() && at > tests_at,
                        "{owner}: {line} outside a last, test-only module"
                    );
                }
                let line: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                assert_eq!(whole, 1, "{owner}: {line}");
                assert_eq!(squeezed.matches(&line).count(), 1, "{owner}: {line}");
                squeezed = squeezed.replace(&line, "");
            }
            if file == "socket.rs" {
                // Its io is the five names it imports, never the module, and
                // it names no type another file could reach the stream by.
                assert!(!squeezed.contains(concat!("io", "::")), "{file} names io");
                assert!(!public(&squeezed, "type"), "{file} aliases");
            }
            for refused in [
                concat!("std::", "{"),
                concat!("std::", "fs"),
                concat!("std::", "os"),
                concat!("std::", "net"),
                concat!("std::", "process"),
                concat!("std::", "env"),
                concat!("std::", "io"),
                concat!("std::", "path"),
                concat!("std::", "time"),
                concat!("std::", "thread"),
                concat!("std", "as"),
                concat!("std::", "*"),
                concat!("::", "r#"),
                concat!("print", "!("),
                concat!("print", "ln!("),
                concat!("dbg", "!("),
                concat!("macro", "_rules!"),
                concat!("::", "//"),
                concat!("ext", "erncrate"),
                concat!("/de", "v/"),
                concat!("SNDRV", "_"),
                concat!("SYS", "_"),
                concat!("cont", "rolC"),
                concat!("as", "m!"),
                concat!("ext", "ern\""),
            ] {
                assert!(!squeezed.contains(refused), "{file} names {refused}");
            }
        }
    }
}
