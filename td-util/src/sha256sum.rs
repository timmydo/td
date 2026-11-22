//! `sha256sum` — SHA-256 digests in coreutils' line format, and `-c` to
//! check them.
//!
//! Installers and release pages publish `HASH  NAME` lines and expect a
//! `sha256sum` on `PATH`. td's root ships no uutils checksum applet (its
//! feature pulls a crate closure td does not take), so this serves the name
//! from the engine's own SHA-256, the copy source preparation trusts.
//!
//! A digest line is the 64 lowercase hex digits, a space, ` ` for text or
//! `*` for binary mode (the two are the same bytes on Linux), and the name.
//! A name holding a backslash, a newline or a carriage return is written
//! escaped (`\\`, `\n`, `\r`) behind a leading `\`, as coreutils does.
//! `-` or no operand is standard input. Options may follow operands, as
//! coreutils permutes them; `--` ends them.
//!
//! `-c` also reads a single space or a tab before the name, leading
//! blanks, and `SHA256 (NAME) = HEX` tag lines, and skips blank and `#`
//! lines. td-util takes its arguments as UTF-8, so an operand that was
//! not is refused rather than replaced: it would name another file.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;

use crate::sha256::Sha256;

const USAGE: &str = "usage: sha256sum [-b|-t] [FILE]...\n       \
                     sha256sum -c [--ignore-missing] [--quiet] [--status] [--strict] [-w] \
                     [FILE]...";
const VERSION: &str = "sha256sum (td-util)";
/// Bytes read per call while hashing or reading a check file.
const CHUNK: usize = 64 * 1024;
/// The longest check line read whole: a name of PATH_MAX escaped to its
/// double, the digest and the separators.
const MAX_LINE: usize = 2 * 4096 + 64 + 3;
const HEX: &[u8; 16] = b"0123456789abcdef";
const TAG: &[u8] = b"SHA256 (";
const TAG_DIGEST: &[u8] = b") = ";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Options {
    check: bool,
    binary: bool,
    quiet: bool,
    status: bool,
    warn: bool,
    strict: bool,
    ignore_missing: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Parsed<'a> {
    Run(Options, Vec<&'a str>),
    Help,
    Version,
}

/// Where a failure lies: the operand's own file, or standard output.
#[derive(Debug)]
enum Failure {
    Operand(io::Error),
    Output(io::Error),
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let (options, operands) = match parse(args)? {
        Parsed::Run(options, operands) => (options, operands),
        Parsed::Help => return Ok(u8::from(writeln!(out, "{USAGE}").is_err())),
        Parsed::Version => return Ok(u8::from(writeln!(out, "{VERSION}").is_err())),
    };
    let stderr = io::stderr();
    Ok(sum(options, &operands, &mut out, &mut stderr.lock()))
}

/// Every operand's line or check, and the status.
fn sum(options: Options, operands: &[&str], out: &mut impl Write, err: &mut impl Write) -> u8 {
    let stdin = ["-"];
    let operands = if operands.is_empty() {
        &stdin[..]
    } else {
        operands
    };
    let mut status = 0u8;
    for operand in operands {
        if operand.contains(char::REPLACEMENT_CHARACTER) {
            report(
                err,
                &[operand.as_bytes()],
                format_args!(": not a UTF-8 name, which td-util cannot pass on exactly"),
            );
            status = 1;
            continue;
        }
        let result = if options.check {
            check(operand, options, out, err)
        } else {
            digest_line(operand, options.binary, out)
        };
        match result {
            Ok(code) => status = status.max(code),
            Err(Failure::Operand(e)) => {
                report(err, &[operand.as_bytes()], format_args!(": {e}"));
                status = 1;
            }
            Err(Failure::Output(e)) => return output_failed(options, status, &e, err),
        }
    }
    match out.flush() {
        Ok(()) => status,
        Err(e) => output_failed(options, status, &e, err),
    }
}

/// Standard output failed. A reader that went away ends a listing quietly,
/// as td-util's other applets do; a check cut short is never a success.
fn output_failed(options: Options, status: u8, error: &io::Error, err: &mut impl Write) -> u8 {
    if error.kind() != io::ErrorKind::BrokenPipe {
        report(err, &[b"standard output"], format_args!(": {error}"));
        return 1;
    }
    if options.check {
        1
    } else {
        status
    }
}

/// The options and operands. Options may follow operands; `--` ends them,
/// and `-` is an operand.
fn parse(args: &[String]) -> Result<Parsed<'_>, String> {
    let oom = |_| message("out of memory");
    let mut options = Options::default();
    let mut mode = false;
    let mut operands = crate::allocation::with_capacity(args.len()).map_err(oom)?;
    let mut ended = false;
    for arg in args {
        let arg = arg.as_str();
        if ended || arg == "-" || !arg.starts_with('-') {
            crate::allocation::push(&mut operands, arg).map_err(oom)?;
            continue;
        }
        match arg {
            "--" => ended = true,
            "--check" => options.check = true,
            "--binary" | "--text" => {
                options.binary = arg == "--binary";
                mode = true;
            }
            "--quiet" => options.quiet = true,
            "--status" => options.status = true,
            "--warn" => options.warn = true,
            "--strict" => options.strict = true,
            "--ignore-missing" => options.ignore_missing = true,
            "--help" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            long if long.starts_with("--") => return Err(message(USAGE)),
            short => {
                for flag in short.bytes().skip(1) {
                    match flag {
                        b'c' => options.check = true,
                        b'b' | b't' => {
                            options.binary = flag == b'b';
                            mode = true;
                        }
                        b'w' => options.warn = true,
                        _ => return Err(message(USAGE)),
                    }
                }
            }
        }
    }
    if mode && options.check {
        return Err(message(
            "the --binary and --text options are meaningless when verifying checksums",
        ));
    }
    let check_only =
        options.quiet || options.status || options.warn || options.strict || options.ignore_missing;
    if check_only && !options.check {
        return Err(message(USAGE));
    }
    Ok(Parsed::Run(options, operands))
}

/// Hash `name`, `-` being standard input, and write its line.
fn digest_line(name: &str, binary: bool, out: &mut impl Write) -> Result<u8, Failure> {
    let digest = hex(&hash_path(name.as_bytes()).map_err(Failure::Operand)?);
    write_digest_line(&digest, name.as_bytes(), binary, out).map_err(Failure::Output)?;
    Ok(0)
}

fn write_digest_line(
    digest: &[u8; 64],
    name: &[u8],
    binary: bool,
    out: &mut impl Write,
) -> io::Result<()> {
    let escaped = needs_escape(name);
    if escaped {
        out.write_all(b"\\")?;
    }
    out.write_all(digest)?;
    out.write_all(if binary { b" *" } else { b"  " })?;
    if escaped {
        write_escaped(name, out)?;
    } else {
        out.write_all(name)?;
    }
    out.write_all(b"\n")
}

fn hash_path(name: &[u8]) -> io::Result<[u8; 32]> {
    if name == b"-" {
        hash_reader(io::stdin().lock())
    } else {
        hash_reader(File::open(OsStr::from_bytes(name))?)
    }
}

fn hash_reader(mut reader: impl Read) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; CHUNK];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(hasher.finalize()),
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(buffer.get(..read).unwrap_or_default());
    }
}

fn hex(digest: &[u8; 32]) -> [u8; 64] {
    let mut text = [0u8; 64];
    for ([high, low], byte) in text.as_chunks_mut::<2>().0.iter_mut().zip(digest) {
        *high = HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'0');
        *low = HEX.get(usize::from(byte & 0xf)).copied().unwrap_or(b'0');
    }
    text
}

fn needs_escape(name: &[u8]) -> bool {
    name.iter()
        .any(|byte| matches!(byte, b'\\' | b'\n' | b'\r'))
}

fn write_escaped(name: &[u8], out: &mut impl Write) -> io::Result<()> {
    for byte in name {
        match byte {
            b'\\' => out.write_all(b"\\\\")?,
            b'\n' => out.write_all(b"\\n")?,
            b'\r' => out.write_all(b"\\r")?,
            other => out.write_all(std::slice::from_ref(other))?,
        }
    }
    Ok(())
}

fn trim_blanks(line: &[u8]) -> &[u8] {
    let blanks = line
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    line.get(blanks..).unwrap_or_default()
}

/// Exactly 64 hex digits, either case, as the digest they spell.
fn digest_of(digits: &[u8]) -> Option<[u8; 32]> {
    if digits.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (byte, [high, low]) in digest.iter_mut().zip(digits.as_chunks::<2>().0) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(digest)
}

fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// What a check line asks, its leading blanks taken: the digest it states
/// and the name, unescaped into `name`. None for a line that is not one.
fn parse_line(line: &[u8], name: &mut Vec<u8>) -> Option<[u8; 32]> {
    name.clear();
    let (escaped, line) = match line.strip_prefix(b"\\") {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (digest, raw) = if let Some(tagged) = line.strip_prefix(TAG) {
        let at = tagged
            .windows(TAG_DIGEST.len())
            .rposition(|window| window == TAG_DIGEST)?;
        let digits = tagged.get(at + TAG_DIGEST.len()..)?;
        (digest_of(digits)?, tagged.get(..at)?)
    } else {
        let (digits, rest) = line.split_at_checked(64)?;
        let raw = match rest {
            [b' ' | b'\t', b' ' | b'*', raw @ ..] | [b' ' | b'\t', raw @ ..] => raw,
            _ => return None,
        };
        (digest_of(digits)?, raw)
    };
    if raw.is_empty() {
        return None;
    }
    let mut bytes = raw.iter();
    while let Some(&byte) = bytes.next() {
        let byte = match (escaped, byte) {
            (true, b'\\') => match bytes.next()? {
                b'\\' => b'\\',
                b'n' => b'\n',
                b'r' => b'\r',
                _ => return None,
            },
            (_, byte) => byte,
        };
        crate::allocation::push(name, byte).ok()?;
    }
    Some(digest)
}

/// Check each line of `list`, `-` being standard input: `NAME: OK` or
/// `NAME: FAILED`, as `options` allow, and 1 for any mismatch, unreadable
/// file, or a list with no line to check.
fn check(
    list: &str,
    options: Options,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<u8, Failure> {
    if list == "-" {
        check_reader(list, true, io::stdin().lock(), options, out, err)
    } else {
        let file = File::open(list).map_err(Failure::Operand)?;
        check_reader(list, false, file, options, out, err)
    }
}

#[derive(Default)]
struct Tally {
    proper: u64,
    verified: u64,
    failed: u64,
    unreadable: u64,
    improper: u64,
}

struct Checker<'a> {
    list: &'a str,
    /// The list is standard input, so a `-` in it names nothing to hash.
    from_stdin: bool,
    options: Options,
    name: Vec<u8>,
    tally: Tally,
}

fn check_reader(
    list: &str,
    from_stdin: bool,
    mut reader: impl Read,
    options: Options,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<u8, Failure> {
    let oom = |_| Failure::Operand(io::Error::from(io::ErrorKind::OutOfMemory));
    let mut line: Vec<u8> = crate::allocation::with_capacity(MAX_LINE).map_err(oom)?;
    let mut checker = Checker {
        list,
        from_stdin,
        options,
        name: crate::allocation::with_capacity(MAX_LINE).map_err(oom)?,
        tally: Tally::default(),
    };
    let mut chunk = [0u8; CHUNK];
    let mut overlong = false;
    let mut number = 0u64;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Failure::Operand(e)),
        };
        let data = chunk.get(..read).unwrap_or_default();
        let mut pieces = data.split(|byte| *byte == b'\n').peekable();
        while let Some(piece) = pieces.next() {
            let complete = pieces.peek().is_some() || read == 0;
            let room = MAX_LINE.saturating_sub(line.len());
            if piece.len() > room {
                overlong = true;
            }
            let kept = piece.get(..room.min(piece.len())).unwrap_or_default();
            crate::allocation::extend_from_slice(&mut line, kept).map_err(oom)?;
            if !complete {
                continue;
            }
            if !(read == 0 && line.is_empty()) {
                number += 1;
                let text = line.strip_suffix(b"\r").unwrap_or(&line);
                checker
                    .line(text, number, overlong, out, err)
                    .map_err(Failure::Output)?;
            }
            line.clear();
            overlong = false;
        }
        if read == 0 {
            break;
        }
    }
    Ok(checker.finish(err))
}

impl Checker<'_> {
    fn line(
        &mut self,
        line: &[u8],
        number: u64,
        overlong: bool,
        out: &mut impl Write,
        err: &mut impl Write,
    ) -> io::Result<()> {
        let line = trim_blanks(line);
        if line.starts_with(b"#") || (line.is_empty() && !overlong) {
            return Ok(());
        }
        // Only its head was read, so what it asks is unknown: it fails
        // rather than passing for blank or malformed.
        if overlong {
            self.tally.proper += 1;
            self.tally.unreadable += 1;
            if !self.options.status {
                report(
                    err,
                    &[self.list.as_bytes()],
                    format_args!(": {number}: a line longer than {MAX_LINE} bytes, not checked"),
                );
            }
            return Ok(());
        }
        let Some(expected) = parse_line(line, &mut self.name) else {
            self.improper(number, err);
            return Ok(());
        };
        if self.from_stdin && self.name == b"-" {
            self.improper(number, err);
            return Ok(());
        }
        self.tally.proper += 1;
        let quiet = self.options.quiet || self.options.status;
        let verdict: &[u8] = match hash_path(&self.name) {
            Ok(actual) if actual == expected => {
                self.tally.verified += 1;
                if quiet {
                    return Ok(());
                }
                b": OK\n"
            }
            Ok(_) => {
                self.tally.verified += 1;
                self.tally.failed += 1;
                b": FAILED\n"
            }
            Err(e) if self.options.ignore_missing && e.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(e) => {
                self.tally.unreadable += 1;
                if !self.options.status {
                    report(err, &[self.name.as_slice()], format_args!(": {e}"));
                }
                b": FAILED open or read\n"
            }
        };
        if self.options.status {
            return Ok(());
        }
        // coreutils escapes a status line's name only for a newline in it.
        if self.name.contains(&b'\n') {
            out.write_all(b"\\")?;
            write_escaped(&self.name, out)?;
        } else {
            out.write_all(&self.name)?;
        }
        out.write_all(verdict)
    }

    fn improper(&mut self, number: u64, err: &mut impl Write) {
        self.tally.improper += 1;
        if self.options.warn && !self.options.status {
            report(
                err,
                &[self.list.as_bytes()],
                format_args!(": {number}: improperly formatted SHA256 checksum line"),
            );
        }
    }

    /// The list's summary warnings, and its status.
    fn finish(&self, err: &mut impl Write) -> u8 {
        let (tally, options) = (&self.tally, self.options);
        let list = self.list.as_bytes();
        if tally.proper == 0 {
            report(
                err,
                &[list],
                format_args!(": no properly formatted SHA256 checksum lines found"),
            );
            return 1;
        }
        if !options.status {
            for (count, one, many) in [
                (
                    tally.improper,
                    "line is improperly formatted",
                    "lines are improperly formatted",
                ),
                (
                    tally.unreadable,
                    "listed file could not be read",
                    "listed files could not be read",
                ),
                (
                    tally.failed,
                    "computed checksum did NOT match",
                    "computed checksums did NOT match",
                ),
            ] {
                if count > 0 {
                    let text = if count == 1 { one } else { many };
                    report(err, &[], format_args!("WARNING: {count} {text}"));
                }
            }
        }
        if options.ignore_missing && tally.verified == 0 && tally.unreadable == 0 {
            if !options.status {
                report(err, &[list], format_args!(": no file was verified"));
            }
            return 1;
        }
        u8::from(tally.failed > 0 || tally.unreadable > 0 || (options.strict && tally.improper > 0))
    }
}

/// `sha256sum: PREFIX...DETAIL` on `err`; nowhere is left to report a
/// failure to write it.
fn report(err: &mut impl Write, prefix: &[&[u8]], detail: fmt::Arguments<'_>) {
    let _ = err.write_all(b"sha256sum: ");
    for part in prefix {
        let _ = err.write_all(part);
    }
    let _ = err.write_fmt(detail);
    let _ = err.write_all(b"\n");
}

/// An error message for `main`, or an empty one when even that cannot be
/// allocated.
fn message(text: &str) -> String {
    crate::allocation::string(text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn check_options() -> Options {
        Options {
            check: true,
            ..Options::default()
        }
    }

    /// Checks `list` as a file named `l`: the status, stdout and stderr.
    fn checked(list: &str, options: Options) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status =
            check_reader("l", false, list.as_bytes(), options, &mut out, &mut err).unwrap();
        (
            status,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn options_may_follow_operands_until_double_dash() {
        let a = args(&["-c", "list", "--quiet", "-w"]);
        let Parsed::Run(options, operands) = parse(&a).unwrap() else {
            panic!("not a run");
        };
        assert!(options.check && options.quiet && options.warn && !options.binary);
        assert_eq!(operands, ["list"]);
        let a = args(&["a", "-b", "--", "-c", "-"]);
        let Parsed::Run(options, operands) = parse(&a).unwrap() else {
            panic!("not a run");
        };
        assert!(options.binary && !options.check);
        assert_eq!(operands, ["a", "-c", "-"]);
        assert_eq!(parse(&args(&["x", "--help"])), Ok(Parsed::Help));
        assert_eq!(parse(&args(&["--version"])), Ok(Parsed::Version));
        for bad in [
            &["-x"][..],
            &["--nope"],
            &["--quiet", "f"],
            &["--status"],
            &["--ignore-missing"],
            &["-cb"],
            &["-c", "--text"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_digest_line_is_coreutils_format() {
        let dir = td_fs::TempDir::new("sha256sum-line").unwrap();
        let file = dir.join("a b");
        std::fs::write(&file, b"abc").unwrap();
        let name = file.to_str().unwrap();
        let mut out = Vec::new();
        digest_line(name, false, &mut out).unwrap();
        assert_eq!(out, format!("{ABC}  {name}\n").into_bytes());
        out.clear();
        digest_line(name, true, &mut out).unwrap();
        assert_eq!(out, format!("{ABC} *{name}\n").into_bytes());
        // A name with a backslash or newline is escaped behind a `\`.
        let odd = dir.join("x\\y\nz");
        std::fs::write(&odd, b"").unwrap();
        out.clear();
        digest_line(odd.to_str().unwrap(), false, &mut out).unwrap();
        let shown = format!("{}", dir.display());
        assert_eq!(out, format!("\\{EMPTY}  {shown}/x\\\\y\\nz\n").into_bytes());
        let missing = digest_line(dir.join("missing").to_str().unwrap(), false, &mut out);
        assert!(matches!(missing, Err(Failure::Operand(_))));
    }

    /// A failed write is standard output's, not the operand's: it ends a
    /// listing quietly when the reader went away, and fails a check.
    #[test]
    fn an_output_failure_is_standard_outputs_and_fails_a_check() {
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let dir = td_fs::TempDir::new("sha256sum-gone").unwrap();
        let file = dir.join("f");
        std::fs::write(&file, b"abc").unwrap();
        let name = file.to_str().unwrap();
        assert!(matches!(
            digest_line(name, false, &mut Gone),
            Err(Failure::Output(_))
        ));
        let mut err = Vec::new();
        assert_eq!(sum(Options::default(), &[name], &mut Gone, &mut err), 0);
        let list = dir.join("list");
        std::fs::write(&list, format!("{ABC}  {name}\n")).unwrap();
        let list = list.to_str().unwrap();
        assert_eq!(sum(check_options(), &[list], &mut Gone, &mut err), 1);
        assert!(err.is_empty());
    }

    #[test]
    fn hashing_streams_across_chunks() {
        let data: Vec<u8> = (0u32..(3 * CHUNK as u32 + 7))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut one = Sha256::new();
        one.update(&data);
        assert_eq!(hash_reader(&data[..]).unwrap(), one.finalize());
    }

    #[test]
    fn check_lines_parse_and_unescape() {
        let mut name = Vec::new();
        for (line, want) in [
            (format!("{ABC}  a b"), &b"a b"[..]),
            (format!("{}  *x", ABC.to_uppercase()), b"*x"),
            (format!("{ABC} *x"), b"x"),
            (format!("{ABC} x"), b"x"),
            (format!("{ABC}\tx"), b"x"),
            (format!("\\{ABC}  a\\\\b\\nc"), b"a\\b\nc"),
            (format!("SHA256 (a) = b) = {ABC}"), b"a) = b"),
            (format!("\\SHA256 (a\\nb) = {ABC}"), b"a\nb"),
        ] {
            assert!(parse_line(line.as_bytes(), &mut name).is_some(), "{line:?}");
            assert_eq!(name, want, "{line:?}");
        }
        for bad in [
            ABC.to_string(),
            format!("{ABC}  "),
            format!("{ABC} "),
            format!("{ABC}x a"),
            format!("{}g  a", &ABC[..63]),
            format!("{}  a", &ABC[..62]),
            format!("\\{ABC}  a\\q"),
            format!("SHA256 () = {ABC}"),
            format!("SHA256 (a) = {}", &ABC[..63]),
            format!("MD5 (a) = {ABC}"),
            String::from("garbage"),
        ] {
            assert!(parse_line(bad.as_bytes(), &mut name).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn check_reports_ok_failed_and_unreadable() {
        let dir = td_fs::TempDir::new("sha256sum-check").unwrap();
        let good = dir.join("good");
        let bad = dir.join("bad");
        std::fs::write(&good, b"abc").unwrap();
        std::fs::write(&bad, b"abd").unwrap();
        let (good, bad) = (good.to_str().unwrap(), bad.to_str().unwrap());
        let missing = dir.join("missing");
        let missing = missing.to_str().unwrap();
        let list = format!(
            "# a comment\n{ABC}  {good}\r\n  {ABC} {bad}\nnot a line\n{ABC}  {missing}\n\n\r\n\
             SHA256 ({good}) = {ABC}"
        );
        let (status, out, err) = checked(&list, check_options());
        assert_eq!(status, 1);
        assert_eq!(
            out,
            format!("{good}: OK\n{bad}: FAILED\n{missing}: FAILED open or read\n{good}: OK\n")
        );
        // Comments and blank lines are not counted improper.
        assert!(err.contains(&format!("sha256sum: {missing}: ")), "{err}");
        assert!(
            err.ends_with(
                "sha256sum: WARNING: 1 line is improperly formatted\n\
                 sha256sum: WARNING: 1 listed file could not be read\n\
                 sha256sum: WARNING: 1 computed checksum did NOT match\n"
            ),
            "{err}"
        );
        // -w names the improper line; --strict fails on it.
        let list = format!("{ABC}  {good}\nnot a line\n");
        let warn = Options {
            warn: true,
            ..check_options()
        };
        let (status, _, err) = checked(&list, warn);
        assert_eq!(status, 0);
        assert!(
            err.starts_with("sha256sum: l: 2: improperly formatted SHA256 checksum line\n"),
            "{err}"
        );
        let strict = Options {
            strict: true,
            ..check_options()
        };
        assert_eq!(checked(&list, strict).0, 1);
        // All good, quietly: no output and success.
        let quiet = Options {
            quiet: true,
            ..check_options()
        };
        assert_eq!(
            checked(&format!("{ABC}  {good}\n"), quiet),
            (0, String::new(), String::new())
        );
        // --status prints nothing, even for a mismatch.
        let status_only = Options {
            status: true,
            ..check_options()
        };
        assert_eq!(
            checked(&format!("{ABC}  {bad}\n{ABC}  {missing}\n"), status_only),
            (1, String::new(), String::new())
        );
        // No line to check at all, or an empty list, is a failure.
        for list in ["junk\n", "", "# only\n"] {
            let (status, out, err) = checked(list, check_options());
            assert_eq!(status, 1, "{list:?}");
            assert!(out.is_empty());
            assert_eq!(
                err,
                "sha256sum: l: no properly formatted SHA256 checksum lines found\n"
            );
        }
    }

    #[test]
    fn ignore_missing_skips_absent_files_but_not_an_all_absent_list() {
        let dir = td_fs::TempDir::new("sha256sum-missing").unwrap();
        let good = dir.join("good");
        std::fs::write(&good, b"abc").unwrap();
        let good = good.to_str().unwrap();
        let missing = dir.join("missing");
        let missing = missing.to_str().unwrap();
        let ignore = Options {
            ignore_missing: true,
            ..check_options()
        };
        let (status, out, err) = checked(&format!("{ABC}  {missing}\n{ABC}  {good}\n"), ignore);
        assert_eq!(
            (status, out.as_str()),
            (0, format!("{good}: OK\n").as_str())
        );
        assert!(err.is_empty(), "{err}");
        let (status, out, err) = checked(&format!("{ABC}  {missing}\n"), ignore);
        assert_eq!((status, out.as_str()), (1, ""));
        assert_eq!(err, "sha256sum: l: no file was verified\n");
    }

    /// Only a newline makes coreutils escape a check's status line.
    #[test]
    fn a_status_line_escapes_only_a_name_with_a_newline() {
        let dir = td_fs::TempDir::new("sha256sum-status-escape").unwrap();
        for (file, escaped) in [("back\\slash", false), ("c\rr", false), ("new\nline", true)] {
            let path = dir.join(file);
            std::fs::write(&path, b"abc").unwrap();
            let mut line = Vec::new();
            write_digest_line(
                &hex(&hash_path(path.as_os_str().as_bytes()).unwrap()),
                path.as_os_str().as_bytes(),
                false,
                &mut line,
            )
            .unwrap();
            let (status, out, _) = checked(&String::from_utf8(line).unwrap(), check_options());
            assert_eq!(status, 0, "{file:?}");
            let shown = if escaped {
                let mut text = Vec::new();
                write_escaped(path.as_os_str().as_bytes(), &mut text).unwrap();
                format!("\\{}", String::from_utf8(text).unwrap())
            } else {
                path.to_str().unwrap().to_string()
            };
            assert_eq!(out, format!("{shown}: OK\n"), "{file:?}");
        }
    }

    /// A list read from standard input cannot also name it: that line is
    /// improper, as coreutils has it, rather than a read of stdin while
    /// stdin is being read.
    #[test]
    fn a_dash_in_a_list_from_stdin_is_improper() {
        let list = format!("{ABC}  -\n");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = check_reader(
            "-",
            true,
            list.as_bytes(),
            check_options(),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(status, 1);
        assert!(out.is_empty());
    }

    /// A line too long to read whole fails, whatever its head shows: a
    /// checksum line, garbage, or blanks hiding one past the bound. Only a
    /// comment is skipped. The next line still counts.
    #[test]
    fn an_overlong_line_fails_and_the_next_counts() {
        let dir = td_fs::TempDir::new("sha256sum-long").unwrap();
        let good = dir.join("good");
        std::fs::write(&good, b"abc").unwrap();
        let good = good.to_str().unwrap();
        let pad = MAX_LINE + CHUNK;
        for head in [
            "a".repeat(pad),
            format!("{ABC}  {}", "n".repeat(pad)),
            format!("{}{ABC}  b", " ".repeat(pad)),
            format!("{}{ABC}  b", " ".repeat(MAX_LINE - 60)),
        ] {
            let long = format!("{head}\n{ABC}  {good}\n");
            let (status, out, err) = checked(&long, check_options());
            assert_eq!(
                (status, out),
                (1, format!("{good}: OK\n")),
                "{}",
                head.len()
            );
            assert!(err.contains("l: 1: a line longer than"), "{err}");
        }
        let long = format!("#{}\n{ABC}  {good}\n", "c".repeat(pad));
        assert_eq!(checked(&long, check_options()).0, 0);
    }

    /// A line split across read chunks is read whole.
    #[test]
    fn a_check_line_may_straddle_chunks() {
        let dir = td_fs::TempDir::new("sha256sum-straddle").unwrap();
        let good = dir.join("good");
        std::fs::write(&good, b"abc").unwrap();
        let good = good.to_str().unwrap();
        let pad = "x".repeat(CHUNK - 10);
        let list = format!("#{pad}\n{ABC}  {good}\n");
        let (status, out, _) = checked(&list, check_options());
        assert_eq!((status, out), (0, format!("{good}: OK\n")));
    }
}
