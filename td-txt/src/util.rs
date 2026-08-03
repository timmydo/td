//! Shared plumbing for the applets: byte-oriented argv, whole-file input, and a
//! buffered stdout that treats a closed pipe as "stop", not as a hard error.

use std::ffi::OsString;
use std::io::{Read, Write};
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

pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(bytes))
}

/// Lossy rendering for a diagnostic. Errors go to a terminal, not to a parser.
pub fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

/// Read a whole file, or stdin for `-`. Applets read whole inputs: both grep and
/// sed need arbitrary lookahead within a file, and the largest thing td's image
/// greps is a source tree.
pub fn read_input(path: &[u8]) -> std::io::Result<Vec<u8>> {
    if path == b"-" {
        let mut buf = Vec::new();
        std::io::stdin().lock().read_to_end(&mut buf)?;
        return Ok(buf);
    }
    std::fs::read(path_from_bytes(path))
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
    if path == b"-" && !from_walk {
        let mut buf = Vec::new();
        return match std::io::stdin().lock().read_to_end(&mut buf) {
            Ok(_) => Ok(buf),
            Err(err) => Err(ReadFail { err, opened: true, partial: buf }),
        };
    }
    let mut file = match std::fs::File::open(path_from_bytes(path)) {
        Ok(f) => f,
        Err(err) => return Err(ReadFail { err, opened: false, partial: Vec::new() }),
    };
    // Size the buffer from the metadata, as `fs::read` does -- but CLAMPED. That
    // number comes from the filesystem and need not be a size anything can hold
    // (a sparse file, `/proc/kcore`), and `with_capacity` ABORTS rather than
    // failing, where the read below reports out of memory and grep carries on.
    const HINT_CAP: usize = 8 << 20;
    let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut buf = Vec::with_capacity(size.saturating_add(1).min(HINT_CAP));
    match file.read_to_end(&mut buf) {
        Ok(_) => Ok(buf),
        Err(err) => Err(ReadFail { err, opened: true, partial: buf }),
    }
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

    #[test]
    fn number_renders_decimal() {
        assert_eq!(number(0), b"0".to_vec());
        assert_eq!(number(1207), b"1207".to_vec());
    }
}
