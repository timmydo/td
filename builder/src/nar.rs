//! NAR (Nix ARchive) serialization, bit-for-bit compatible with the pinned
//! daemon / (guix serialization) write-file — the S2 oracle semantics, read
//! off the pin and recorded in plan/td-builder.md:
//!   - tokens and contents are framed as u64 little-endian length + bytes,
//!     zero-padded to the next 8-byte boundary;
//!   - directory entries are sorted in codepoint order ("." and ".." never
//!     appear: read_dir does not yield them);
//!   - a regular file is "executable" iff (mode & 0o100);
//!   - symlink targets are written verbatim (readlink, no resolution).
//! The serialization streams into any Write — the nar-hash CLI wires it to
//! the SHA-256 hasher so file contents are never buffered whole.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn write_framed(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    out.write_all(&(bytes.len() as u64).to_le_bytes())?;
    out.write_all(bytes)?;
    let pad = (8 - bytes.len() % 8) % 8;
    out.write_all(&[0u8; 8][..pad])
}

fn write_token(out: &mut impl Write, s: &str) -> io::Result<()> {
    write_framed(out, s.as_bytes())
}

/// Frame a regular file's contents: length header, streamed bytes, padding.
fn write_contents(out: &mut impl Write, path: &Path, len: u64) -> io::Result<()> {
    write_token(out, "contents")?;
    out.write_all(&len.to_le_bytes())?;
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 65536];
    let mut copied: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        copied += n as u64;
        out.write_all(&buf[..n])?;
    }
    // The length was framed before streaming; a file that changed size under
    // us would silently corrupt the archive — refuse instead.
    if copied != len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: size changed during read ({} != {})", path.display(), copied, len),
        ));
    }
    let pad = (8 - (len % 8) as usize) % 8;
    out.write_all(&[0u8; 8][..pad])
}

fn write_node(out: &mut impl Write, path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    write_token(out, "(")?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        write_token(out, "type")?;
        write_token(out, "symlink")?;
        write_token(out, "target")?;
        let target = fs::read_link(path)?;
        write_framed(out, target.as_os_str().as_encoded_bytes())?;
    } else if ft.is_dir() {
        write_token(out, "type")?;
        write_token(out, "directory")?;
        let mut entries: Vec<Vec<u8>> = fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name().as_encoded_bytes().to_vec()))
            .collect::<io::Result<_>>()?;
        entries.sort();
        for name in entries {
            write_token(out, "entry")?;
            write_token(out, "(")?;
            write_token(out, "name")?;
            write_framed(out, &name)?;
            write_token(out, "node")?;
            // OsStr round-trip keeps non-UTF-8 names intact.
            let child = path.join(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&name) });
            write_node(out, &child)?;
            write_token(out, ")")?;
        }
    } else if ft.is_file() {
        write_token(out, "type")?;
        write_token(out, "regular")?;
        if meta.permissions().mode() & 0o100 != 0 {
            write_token(out, "executable")?;
            write_token(out, "")?;
        }
        write_contents(out, path, meta.len())?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: unsupported file type for NAR", path.display()),
        ));
    }
    write_token(out, ")")
}

/// Serialize PATH as a NAR into OUT.
pub fn write_nar(out: &mut impl Write, path: &Path) -> io::Result<()> {
    write_token(out, "nix-archive-1")?;
    write_node(out, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(s: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        write_framed(&mut v, s).unwrap();
        v
    }

    #[test]
    fn framing_pads_to_eight() {
        // 3 bytes -> 8-byte LE length + 3 bytes + 5 zeros.
        let v = framed(b"abc");
        assert_eq!(v.len(), 8 + 8);
        assert_eq!(&v[..8], &3u64.to_le_bytes());
        assert_eq!(&v[8..11], b"abc");
        assert_eq!(&v[11..], &[0u8; 5]);
        // Exact multiples take no padding; empty takes none.
        assert_eq!(framed(b"12345678").len(), 16);
        assert_eq!(framed(b"").len(), 8);
    }

    #[test]
    fn known_nar_of_single_file() {
        // NAR of a lone regular file "x" with contents "hi\n": the byte layout
        // is fully determined, so assert it token by token.
        let dir = std::env::temp_dir().join(format!("td-nar-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x");
        fs::write(&f, b"hi\n").unwrap();
        let mut got = Vec::new();
        write_nar(&mut got, &f).unwrap();
        let mut want = Vec::new();
        for t in ["nix-archive-1", "(", "type", "regular", "contents"] {
            want.extend(framed(t.as_bytes()));
        }
        want.extend(framed(b"hi\n"));
        want.extend(framed(b")"));
        assert_eq!(got, want);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_entries_are_sorted_by_byte() {
        // "B" (0x42) must sort before "a" (0x61) — codepoint order, not
        // case-insensitive collation.
        let dir = std::env::temp_dir().join(format!("td-nar-sort-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), b"1").unwrap();
        fs::write(dir.join("B"), b"2").unwrap();
        let mut nar = Vec::new();
        write_nar(&mut nar, &dir).unwrap();
        // A framed 1-byte name is the byte plus 7 zeros of padding.
        let pos_b = nar.windows(8).position(|w| w == b"B\0\0\0\0\0\0\0"[..].as_ref());
        let pos_a = nar.windows(8).position(|w| w == b"a\0\0\0\0\0\0\0"[..].as_ref());
        assert!(pos_b.unwrap() < pos_a.unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
