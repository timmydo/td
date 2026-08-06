//! Shared plumbing for the applets: byte-oriented argv, whole-file input, and a
//! buffered stdout that treats a closed pipe as "stop", not as a hard error.

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Seek, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Program arguments as raw bytes. A pattern or a filename need not be UTF-8, so
/// nothing here goes through `String`.
pub fn args_bytes() -> Vec<Vec<u8>> {
    std::env::args_os().map(|a| a.into_vec()).collect()
}

pub fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

/// Whether `POSIXLY_CORRECT` is set. GNU reads only whether it is THERE -- an
/// empty value, `0` and `no` all count. GNU gives it two jobs; td-txt uses it
/// for ONE, ending option parsing at the first operand. The other, sed's POSIX
/// mode, is deliberately not driven from here -- see spec/README's gap entry,
/// which measures why it is not the same switch as `--posix`.
pub fn posixly_correct() -> bool {
    std::env::var_os("POSIXLY_CORRECT").is_some()
}

pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(bytes))
}

/// A diagnostic with RAW bytes spliced into it, which is what GNU's `%s` writes.
/// A `String` cannot hold one: it is UTF-8 by construction, and a name need not
/// be, so re-encoding one names a file the operator never passed.
pub fn name_in(before: &str, name: &[u8], after: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(before.len() + name.len() + after.len());
    out.extend_from_slice(before.as_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(after.as_bytes());
    out
}

/// The same splice for one byte, which is GNU's `%c`. `format!` cannot do it:
/// `char::from` widens the byte to a Unicode scalar and encodes it as UTF-8, so
/// every byte from 0x80 up arrives as two.
pub fn byte_in(before: &str, byte: u8, after: &str) -> Vec<u8> {
    name_in(before, &[byte], after)
}

/// The prefix a C `%s` prints: the bytes before the first NUL. GNU builds each
/// diagnostic as a C string, so a NUL that reaches one ENDS it -- and only a
/// `-f` script can carry a NUL into a message, argv being NUL-terminated.
pub fn cstr(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|b| *b == 0) {
        Some(n) => bytes.get(..n).unwrap_or_default(),
        None => bytes,
    }
}

/// What `--version` reports. A literal rather than `env!("CARGO_PKG_VERSION")`
/// because the SHIPPED binary is compiled by a direct `rustc src/main.rs` with no
/// cargo to set that variable; `version_matches_the_manifest` pins the two.
pub const VERSION: &str = "0.1.0";

/// An errno as GNU spells it. Rust's `io::Error` Display appends ` (os error N)`
/// to the `strerror` text; these diagnostics are read next to every other tool's,
/// so the suffix comes back off.
pub fn errmsg(e: &std::io::Error) -> String {
    let text = e.to_string();
    let Some(n) = e.raw_os_error() else {
        return text;
    };
    match text.strip_suffix(&format!(" (os error {n})")) {
        Some(head) => head.to_string(),
        None => text,
    }
}

/// An input that is OPEN, so a caller can tell the two failures apart. GNU carries
/// on from a name it could not open -- a warning, exit 2 -- and treats one it
/// opened and could not READ as fatal to sed (`read error on NAME`, exit 4) and as
/// an empty file to grep. A DIRECTORY operand is the reachable case of the second:
/// `open(2)` on one succeeds and the first read fails.
pub enum Input {
    Stdin,
    File(std::fs::File),
}

impl Input {
    /// `dash_is_stdin` is false for a name grep's WALK produced -- those are always
    /// files, so `grep -r` over a directory holding one called `-` searches it --
    /// and for sed's `-i`, which rewrites a NAME and cannot rewrite a pipe.
    pub fn open(path: &[u8], dash_is_stdin: bool) -> std::io::Result<Self> {
        if dash_is_stdin && path == b"-" {
            return Ok(Self::Stdin);
        }
        Ok(Self::File(std::fs::File::open(path_from_bytes(path))?))
    }

    /// Why `-i` may not rewrite this, if it may not -- GNU's two refusals, in GNU's
    /// ORDER, since a terminal is also not a regular file and the first test is what
    /// names it. Asked of the OPEN descriptor rather than of the name, so the answer
    /// cannot be about a different file than the one that will be read. Stdin cannot
    /// arrive here (`-i` opens `-` as the ordinary name it is) and is refused as the
    /// non-file it would be.
    pub fn in_place_refusal(&self) -> Option<&'static str> {
        let Self::File(file) = self else {
            return Some("not a regular file");
        };
        if file.is_terminal() {
            return Some("is a terminal");
        }
        if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
            return Some("not a regular file");
        }
        None
    }

    /// Whether the bytes came off descriptor 0, which decides whether a caller
    /// that over-read may hand the surplus back. Asked of the OPEN input rather
    /// than by re-deciding what `-` meant, as `error_name` is.
    pub fn is_stdin(&self) -> bool {
        matches!(self, Self::Stdin)
    }

    /// How a READ failure names this input. GNU registers the standard input STREAM
    /// under the name `stdin` and reports read errors against that, where the same
    /// operand is `-` to `F` and to the `can't read` warning -- so
    /// `sed p < a-directory` is `read error on stdin`. Answered from the OPEN input
    /// rather than by re-deciding what `-` meant, which is a second place to get it
    /// wrong.
    pub fn error_name(&self, path: &[u8]) -> Vec<u8> {
        match self {
            Self::Stdin => b"stdin".to_vec(),
            Self::File(_) => path.to_vec(),
        }
    }

    /// Read to EOF. On failure the bytes that DID arrive come back with it: a read
    /// can append and then fail, and reporting that file as empty would lose the
    /// matches in what was read.
    pub fn read_all(&mut self) -> Result<Vec<u8>, (std::io::Error, Vec<u8>)> {
        let mut buf = Vec::new();
        let read = match self {
            Self::Stdin => std::io::stdin().lock().read_to_end(&mut buf),
            Self::File(file) => {
                // Size the buffer from the metadata, as `fs::read` does -- but
                // CLAMPED. That number comes from the filesystem and need not be a
                // size anything can hold (a sparse file, `/proc/kcore`), and
                // `with_capacity` ABORTS rather than failing, where the read below
                // reports out of memory and the applet carries on.
                const HINT_CAP: usize = 8 << 20;
                let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
                buf.reserve(size.saturating_add(1).min(HINT_CAP));
                file.read_to_end(&mut buf)
            }
        };
        match read {
            Ok(_) => Ok(buf),
            Err(err) => Err((err, buf)),
        }
    }
}

/// Hand back to descriptor 0 what was read from it and never used, so the next
/// reader of that file description starts where this applet stopped instead of at
/// end of file. It is POSIX's "shall not consume more input than it needs", and
/// what makes `{ sed 1q; cat; } < f` the usable idiom it is meant to be.
///
/// A REGULAR file only: a descriptor can answer a position query and still refuse
/// a negative seek, and the other common stdin -- a pipe -- has nowhere to put
/// what it was given back. Nothing is lost by declining, since not seeking is
/// exactly the behaviour that stands today.
///
/// The seek is RELATIVE, so a process that SHARES this description and moves the
/// offset first sends it somewhere else entirely. That is inherent to reading more
/// than is needed and returning the rest, bash and GNU sed included.
///
/// The dup is what keeps this in safe code: `Stdin` exposes no seek, while an
/// `OwnedFd` cloned from it shares the DESCRIPTION, so seeking the clone is what
/// moves descriptor 0. Dropping the clone closes only the clone.
pub fn give_back_stdin(unread: u64) -> std::io::Result<()> {
    if unread == 0 {
        return Ok(());
    }
    // A dup needs a free descriptor, and `w` targets are still open here, so this
    // can fail with EMFILE on a run that otherwise SUCCEEDED. That is the process
    // fd table being full and says nothing about descriptor 0 -- and failing the
    // run would not recover the offset it could not reach -- so it DECLINES, as
    // the non-regular case does. What stays fatal is the seek, which is the call
    // that does say something about the descriptor.
    let Ok(dup) = std::io::stdin().as_fd().try_clone_to_owned() else {
        return Ok(());
    };
    let mut file = std::fs::File::from(dup);
    if !file.metadata()?.is_file() {
        return Ok(());
    }
    let back = i64::try_from(unread).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "read past what a seek can undo")
    })?;
    file.seek(std::io::SeekFrom::Current(-back))?;
    Ok(())
}

/// Read a whole input, or stdin for `-`, for a caller the two failures read alike
/// to. Applets read whole inputs: both grep and sed need arbitrary lookahead
/// within a file, and the largest thing td's image greps is a source tree.
pub fn read_input(path: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut input = Input::open(path, true)?;
    input.read_all().map_err(|(err, _)| err)
}

/// A read that failed, and whether the OPEN is what failed. GNU counts a file it
/// opened and could not READ as one it processed and found nothing in -- a
/// directory operand is the reachable case, so `grep -c a d` prints its `0` --
/// while a name it could not open at all was never processed.
pub struct ReadFail {
    pub err: std::io::Error,
    pub opened: bool,
    /// What the read DID return before failing. A read can append bytes and then
    /// fail, and those bytes were still read: discarding them would report a file
    /// as empty that had matches in it.
    pub partial: Vec<u8>,
}

/// `read_input` for the SEARCH: it reports where the failure fell, and a name the
/// WALK produced is always a file. Only an operand spells stdin `-`, so `grep -r`
/// in a directory holding a file called `-` searches it rather than reading
/// stdin, as GNU does.
pub fn read_search(path: &[u8], from_walk: bool) -> Result<Vec<u8>, ReadFail> {
    let mut input = match Input::open(path, !from_walk) {
        Ok(input) => input,
        Err(err) => return Err(ReadFail { err, opened: false, partial: Vec::new() }),
    };
    input.read_all().map_err(|(err, partial)| ReadFail { err, opened: true, partial })
}

/// Write one line to stdout, treating a CLOSED READER as "done" rather than as a
/// panic — `println!` aborts the applet thread on EPIPE (`grep --help | head -1`),
/// which every other write here already avoids by going through `Out`, so the
/// one-shot informational outputs go through it too. A genuine write failure is
/// returned rather than turned into a status here: the two applets number their
/// errors differently (grep 2, sed 4), and picking one for them would be wrong for
/// the other.
pub fn print_line(text: &str) -> std::io::Result<()> {
    let mut out = Out::new();
    out.write(text.as_bytes())?;
    out.write(b"\n")?;
    out.flush()
}

/// Buffered stdout. `broken` latches once the reader has gone away so a caller
/// can stop early instead of reporting an I/O failure per line.
pub struct Out {
    inner: std::io::BufWriter<std::io::Stdout>,
    broken: bool,
}

impl Out {
    pub fn new() -> Self {
        Self { inner: std::io::BufWriter::new(std::io::stdout()), broken: false }
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.broken {
            return Ok(());
        }
        match self.inner.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.broken = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        if self.broken {
            return Ok(());
        }
        match self.inner.flush() {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.broken = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Default for Out {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a buffer into records on `sep`, reporting for each whether it ended
/// with the separator. A trailing record without one is still a record.
pub fn records(buf: &[u8], sep: u8) -> Vec<(&[u8], bool)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in buf.iter().enumerate() {
        if *b == sep {
            if let Some(rec) = buf.get(start..i) {
                out.push((rec, true));
            }
            start = i + 1;
        }
    }
    if start < buf.len() {
        if let Some(rec) = buf.get(start..) {
            out.push((rec, false));
        }
    }
    out
}

/// Something the walk has to say about a path.
pub enum Diag {
    /// A directory that could not be read. An error: it sets the exit status.
    Failed(std::io::Error),
    /// A directory skipped for being its own ancestor. GNU WARNS and carries on,
    /// so this one leaves the status to the search.
    Loop,
}

/// What a walk produced.
pub struct Walked {
    /// Files to search, sorted.
    pub files: Vec<PathBuf>,
    /// In the order the walk MET them, both kinds in one list: GNU interleaves
    /// its errors and its cycle warnings, and two lists could only be drained one
    /// after the other.
    pub diags: Vec<(PathBuf, Diag)>,
    /// Whether the root was a directory; the caller names its output on it, and a
    /// second copy of the test would drift the moment this one changes.
    pub descended: bool,
}

/// Directory walk for `grep -r`/`-R`, deepest-last, sorted for a deterministic
/// listing.
///
/// `logical` is `-R`. Without it the walk is PHYSICAL: the root is followed but
/// nothing below it is, so a symlink named as an operand is descended and one
/// found by the walk is skipped, and only REGULAR files are collected — a FIFO
/// there would block the open forever, and GNU skips it. With it every symlink is
/// followed and every non-directory is collected, devices included, which is GNU's
/// `-R` and is why that flag CAN block where `-r` cannot. Following symlinks is the
/// usual way a directory becomes reachable from inside itself, but not the only one
/// -- a bind mount does it without any symlink -- so BOTH walks carry the ancestor
/// chain and refuse to re-enter one, as GNU's does.
///
/// Errors are collected rather than propagated: one 0700 subdirectory must not
/// discard the whole walk, which is what `?` here used to do — `grep -r` then
/// reported nothing at all and blamed the root. GNU searches what it can reach
/// and reports each directory it cannot.
pub fn walk(root: &Path, logical: bool) -> Walked {
    let mut out = Walked { files: Vec::new(), diags: Vec::new(), descended: false };
    out.descended = walk_from(root, true, logical, &mut Vec::new(), &mut out);
    out
}

/// `follow` is true for the operand and false for what the walk finds, which is
/// the asymmetry above; `logical` follows both. It is a parameter rather than a
/// choice of stat per call site so that a directory entry swapped for a symlink
/// between the two cannot be descended.
fn walk_from(
    root: &Path,
    follow: bool,
    logical: bool,
    chain: &mut Vec<(u64, u64)>,
    out: &mut Walked,
) -> bool {
    let meta = match stat(root, follow || logical) {
        Ok(m) => m,
        Err(e) => {
            // A name that EXISTS without RESOLVING — a dangling symlink, a loop —
            // is passed on rather than reported here, because a search that never
            // opens it must stay silent: `grep -rm0 a dangling` is GNU's exit 1
            // with nothing said. The open reports it in every other case.
            match std::fs::symlink_metadata(root).is_ok() {
                true => out.files.push(root.to_path_buf()),
                false => out.diags.push((root.to_path_buf(), Diag::Failed(e))),
            }
            return false;
        }
    };
    if !meta.is_dir() {
        out.files.push(root.to_path_buf());
        return false;
    }
    chain.push(ident(&meta));
    let reader = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            out.diags.push((root.to_path_buf(), Diag::Failed(e)));
            chain.pop();
            return true;
        }
    };
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in reader {
        match entry {
            Ok(e) => entries.push(e.path()),
            Err(e) => out.diags.push((root.to_path_buf(), Diag::Failed(e))),
        }
    }
    entries.sort();
    for path in entries {
        match stat(&path, logical) {
            Ok(meta) if meta.is_dir() => match chain.contains(&ident(&meta)) {
                true => out.diags.push((path, Diag::Loop)),
                false => {
                    walk_from(&path, false, logical, chain, out);
                }
            },
            // A symlink, FIFO, socket or device found by a PHYSICAL walk: GNU
            // skips it. A logical walk reads them, having followed the link.
            Ok(meta) if logical || meta.is_file() => out.files.push(path),
            Ok(_) => {}
            // Only a logical walk gets here, on a link that does not resolve. Passed
            // on for the same reason the root is: the OPEN reports it, so a search
            // that never opens stays silent as GNU's does.
            Err(e) => match std::fs::symlink_metadata(&path).is_ok() {
                true => out.files.push(path),
                false => out.diags.push((path, Diag::Failed(e))),
            },
        }
    }
    chain.pop();
    true
}

fn stat(path: &Path, follow: bool) -> std::io::Result<std::fs::Metadata> {
    match follow {
        true => std::fs::metadata(path),
        false => std::fs::symlink_metadata(path),
    }
}

/// The pair that says "the same directory": a logical walk re-entering one is a
/// cycle, and no path comparison can tell, two names resolving to one inode.
fn ident(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Bytes of a path, for output that must round-trip a non-UTF-8 name.
pub fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

/// Decimal rendering without `format!`'s UTF-8 detour.
pub fn number(n: u64) -> Vec<u8> {
    let mut digits = Vec::new();
    let mut v = n;
    loop {
        digits.push(b'0' + u8::try_from(v % 10).unwrap_or(0));
        v /= 10;
        if v == 0 {
            break;
        }
    }
    digits.reverse();
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus harness always PIPES stdin, so no case can make a read on it
    /// fail; the rule is pinned here instead and checked by hand against GNU.
    #[test]
    fn a_read_error_names_the_standard_input_stream_stdin() {
        let stdin = Input::open(b"-", true).unwrap();
        assert_eq!(stdin.error_name(b"-"), b"stdin".to_vec());
        // The refusal `-i` would give it, though `-i` opens `-` as a name and so
        // never routes the stream here.
        assert_eq!(stdin.in_place_refusal(), Some("not a regular file"));
    }

    /// The whole open/read split rests on `open(2)` SUCCEEDING on a directory and
    /// the first read failing, which is a property of the kernel and of `std`, not
    /// of this crate -- so it is asserted rather than assumed.
    #[test]
    fn a_directory_opens_and_will_not_read() {
        let dir = std::env::temp_dir().join(format!("td-txt-input-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = dir.as_os_str().as_bytes();
        let mut opened = Input::open(name, true).expect("a directory opens");
        assert_eq!(opened.in_place_refusal(), Some("not a regular file"));
        assert_eq!(opened.error_name(name), name.to_vec());
        let err = opened.read_all().expect_err("and does not read").0;
        assert_eq!(errmsg(&err), "Is a directory");

        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        let mut opened = Input::open(file.as_os_str().as_bytes(), true).unwrap();
        assert_eq!(opened.in_place_refusal(), None);
        assert_eq!(opened.read_all().unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_keeps_a_final_unterminated_line() {
        let r = records(b"a\nb\nc", b'\n');
        assert_eq!(r.len(), 3);
        assert_eq!(r.get(2).map(|(l, nl)| (*l, *nl)), Some((&b"c"[..], false)));
    }

    #[test]
    fn records_of_a_terminated_buffer_all_carry_the_separator() {
        let r = records(b"a\nb\n", b'\n');
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|(_, nl)| *nl));
    }

    #[test]
    fn empty_input_has_no_records() {
        assert!(records(b"", b'\n').is_empty());
    }

    #[test]
    fn version_matches_the_manifest() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn errmsg_drops_rusts_os_error_suffix() {
        let e = std::io::Error::from_raw_os_error(2);
        assert!(e.to_string().contains("(os error 2)"));
        assert_eq!(errmsg(&e), "No such file or directory");
        // A non-errno error has no suffix to drop.
        let other = std::io::Error::other("boom");
        assert_eq!(errmsg(&other), "boom");
    }

    /// The FIRST NUL, not the last: a message holding two would otherwise keep
    /// the text between them, which no C `%s` prints.
    #[test]
    fn cstr_stops_at_the_first_nul() {
        assert_eq!(cstr(b"a\0b\0c"), b"a");
        assert_eq!(cstr(b"abc"), b"abc");
        assert_eq!(cstr(b"\0abc"), b"");
        assert_eq!(cstr(b""), b"");
    }

    /// A byte goes in as ITSELF; `char::from` would send 0x80 as two.
    #[test]
    fn byte_in_splices_one_raw_byte() {
        assert_eq!(byte_in("a`", 0x80, "'"), b"a`\x80'".to_vec());
        assert_eq!(byte_in("", 0, ""), vec![0u8]);
    }

    /// `name_in` on its own: `byte_in` only ever hands it a ONE-byte name, and
    /// the call sites also pass an empty prefix (`NAME: why`) and an empty
    /// suffix (`... for NAME`).
    #[test]
    fn name_in_splices_a_raw_name_between_two_pieces() {
        assert_eq!(name_in("a: ", b"n\xffm", " :b"), b"a: n\xffm :b".to_vec());
        assert_eq!(name_in("", b"\xff\xfe", ": why"), b"\xff\xfe: why".to_vec());
        assert_eq!(name_in("for ", b"\x80", ""), b"for \x80".to_vec());
        // A lone continuation byte is the case a re-encoding would destroy, and
        // it survives here as itself rather than as the three of U+FFFD.
        assert_eq!(name_in("", b"\xc3", ""), vec![0xc3]);
        assert_eq!(name_in("x", b"", "y"), b"xy".to_vec());
    }

    #[test]
    fn number_renders_decimal() {
        assert_eq!(number(0), b"0".to_vec());
        assert_eq!(number(1207), b"1207".to_vec());
    }
}
