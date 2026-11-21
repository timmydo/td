//! The PulseAudio native protocol as td speaks it: the pstream framing
//! (`wire`), the tagstruct codec (`tag`), and the captured command table,
//! error codes and version negotiation (`proto`).
//!
//! The normative design is `APPLICATIONS.md` §K.3. `td-audio` serves this
//! protocol on its socket; a control client speaks the same bytes, so the
//! codec lives here once rather than in a copy per consumer. Every module is
//! safe code: the daemon's syscall surface stays in `td-audio`.
#![forbid(unsafe_code)]

pub mod proto;
pub mod tag;
pub mod wire;

#[cfg(test)]
mod tests {
    /// Every source file of this crate, read at compile time.
    fn sources() -> Vec<(&'static str, &'static str)> {
        vec![
            ("lib.rs", include_str!("lib.rs")),
            ("proto.rs", include_str!("proto.rs")),
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

    /// The codec is bytes in and bytes out. No file names an OS facility, a
    /// syscall, a device or the PCM and control requests td-audio refuses
    /// (`UNSAFE.md` §13): the daemon links this crate, so a name here would
    /// reach the shipped binary past td-audio's own scans. Whole files, not
    /// the text before the tests: production code may follow a test module.
    /// Comments are kept, so a `//` inside a string literal hides nothing and
    /// a mention fails closed; a grouped `std` import is refused because it
    /// names a module without any of the paths below. Each name is split so
    /// this list does not match itself.
    #[test]
    fn the_codec_reaches_no_device_or_operating_system_facility() {
        for (file, text) in sources() {
            let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            for refused in [
                concat!("std::", "{"),
                concat!("std::", "fs"),
                concat!("std::", "os"),
                concat!("std::", "net"),
                concat!("std::", "process"),
                concat!("std::", "env"),
                concat!("std::", "io"),
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
