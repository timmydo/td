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

/// Directory walk for `grep -r`, deepest-last, sorted for a deterministic
/// listing. Symlinked directories are not descended (GNU's `-r` behavior);
/// unreadable entries surface as errors to the caller.
/// Every readable file under `root`, plus one error PER unreadable directory.
///
/// Errors are collected rather than propagated: one 0700 subdirectory must not
/// discard the whole walk, which is what `?` here used to do — `grep -r` then
/// reported nothing at all and blamed the root. GNU searches what it can reach
/// and reports each directory it cannot.
pub fn walk(root: &Path, out: &mut Vec<PathBuf>, errs: &mut Vec<(PathBuf, std::io::Error)>) {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) => {
            errs.push((root.to_path_buf(), e));
            return;
        }
    };
    if !meta.is_dir() {
        out.push(root.to_path_buf());
        return;
    }
    let reader = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            errs.push((root.to_path_buf(), e));
            return;
        }
    };
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in reader {
        match entry {
            Ok(e) => entries.push(e.path()),
            Err(e) => errs.push((root.to_path_buf(), e)),
        }
    }
    entries.sort();
    for path in entries {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => walk(&path, out, errs),
            Ok(meta) if !meta.file_type().is_symlink() => out.push(path),
            Ok(_) => {}
            Err(e) => errs.push((path, e)),
        }
    }
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
    fn number_renders_decimal() {
        assert_eq!(number(0), b"0".to_vec());
        assert_eq!(number(1207), b"1207".to_vec());
    }
}
