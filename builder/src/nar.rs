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

// ---- read side: restore a NAR stream back onto disk (the inverse of write_nar) ----
// Used by the substitute consumer to unpack a fetched NAR into the store. The reader is
// the exact mirror of the writer above: same little-endian length + zero-pad framing,
// same node grammar. It is strict on purpose — a truncated or garbled archive (a
// corrupted download) must ERROR, never restore a partial tree, so the caller can fall
// back to building. The NAR hash is verified by the caller against the signed metadata
// before this runs; the bounds here are defence-in-depth against a malformed stream.

/// Cap on a framed token/name/symlink-target read (file contents stream separately, so
/// they are never bound by this): a larger frame means a corrupt or hostile archive.
const MAX_NAR_TOKEN: u64 = 1 << 20;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn read_u64(input: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    input.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Consume the zero padding that follows a `len`-byte frame, asserting it is zero (a
/// non-zero pad is a malformed archive).
fn read_padding(input: &mut impl Read, len: u64) -> io::Result<()> {
    let pad = (8 - (len % 8) as usize) % 8;
    if pad > 0 {
        let mut b = [0u8; 8];
        input.read_exact(&mut b[..pad])?;
        if b[..pad].iter().any(|&x| x != 0) {
            return Err(invalid("non-zero NAR frame padding"));
        }
    }
    Ok(())
}

/// Read one framed token/name/target (small, capped). EOF mid-frame errors.
fn read_framed(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_u64(input)?;
    if len > MAX_NAR_TOKEN {
        return Err(invalid(format!("NAR token of {len} bytes exceeds cap")));
    }
    let mut buf = vec![0u8; len as usize];
    input.read_exact(&mut buf)?;
    read_padding(input, len)?;
    Ok(buf)
}

/// Read a framed token and require it to equal WANT.
fn expect(input: &mut impl Read, want: &str) -> io::Result<()> {
    let got = read_framed(input)?;
    if got != want.as_bytes() {
        return Err(invalid(format!(
            "expected NAR token {want:?}, got {:?}",
            String::from_utf8_lossy(&got)
        )));
    }
    Ok(())
}

/// Stream exactly `n` bytes from INPUT to OUT (read_exact in chunks: a short read at EOF
/// errors, so a truncated contents frame can never be restored as a partial file).
fn copy_n(input: &mut impl Read, out: &mut impl Write, mut n: u64) -> io::Result<()> {
    let mut buf = [0u8; 65536];
    while n > 0 {
        let want = n.min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..want])?;
        out.write_all(&buf[..want])?;
        n -= want as u64;
    }
    Ok(())
}

fn read_node(input: &mut impl Read, path: &Path) -> io::Result<()> {
    expect(input, "(")?;
    expect(input, "type")?;
    match read_framed(input)?.as_slice() {
        b"regular" => {
            // Optional ["executable", ""] precedes "contents".
            let mut tok = read_framed(input)?;
            let exec = tok == b"executable";
            if exec {
                if !read_framed(input)?.is_empty() {
                    return Err(invalid("NAR 'executable' not followed by empty token"));
                }
                tok = read_framed(input)?;
            }
            if tok != b"contents" {
                return Err(invalid("expected NAR 'contents' token"));
            }
            let len = read_u64(input)?;
            let mut f = fs::File::create(path)?;
            copy_n(input, &mut f, len)?;
            read_padding(input, len)?;
            drop(f);
            // Restore only what NAR encodes: the executable bit (mode & 0o100).
            let mode = if exec { 0o755 } else { 0o644 };
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            expect(input, ")")
        }
        b"symlink" => {
            expect(input, "target")?;
            let target = read_framed(input)?;
            // OsStr round-trip keeps a non-UTF-8 target intact (mirror of write_node).
            let target = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&target) };
            std::os::unix::fs::symlink(target, path)?;
            expect(input, ")")
        }
        b"directory" => {
            fs::create_dir(path)?;
            loop {
                match read_framed(input)?.as_slice() {
                    b")" => return Ok(()),
                    b"entry" => {}
                    other => {
                        return Err(invalid(format!(
                            "expected NAR 'entry' or ')', got {:?}",
                            String::from_utf8_lossy(other)
                        )))
                    }
                }
                expect(input, "(")?;
                expect(input, "name")?;
                let name = read_framed(input)?;
                // Reject any name that could escape the directory.
                if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
                    return Err(invalid("unsafe NAR entry name"));
                }
                expect(input, "node")?;
                let child = path.join(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&name) });
                read_node(input, &child)?;
                expect(input, ")")?;
            }
        }
        other => Err(invalid(format!(
            "unknown NAR node type {:?}",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// Restore a NAR stream from INPUT onto DEST (which must not already exist). The inverse
/// of `write_nar`: `write_nar(.., p)` then `read_nar(.., q)` reconstructs the tree at `p`
/// under `q` (same contents, executable bits, symlink targets, directory structure).
pub fn read_nar(input: &mut impl Read, dest: &Path) -> io::Result<()> {
    if read_framed(input)? != b"nix-archive-1" {
        return Err(invalid("not a NAR (bad magic)"));
    }
    read_node(input, dest)
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

    /// Build a small but type-complete tree under DIR: a plain file, an executable
    /// file, a symlink, and a nested directory with its own file.
    fn build_tree(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("a"), b"plain\n").unwrap();
        let run = dir.join("run");
        fs::write(&run, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a", dir.join("lnk")).unwrap();
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("nested"), b"deep\n").unwrap();
    }

    #[test]
    fn read_nar_round_trips_a_tree() {
        // write_nar(tree) -> read_nar -> the reconstruction re-serializes to the SAME
        // NAR. This is the durable inverse-property check: no Guix oracle in the room.
        let base = std::env::temp_dir().join(format!("td-nar-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        build_tree(&src);

        let mut nar = Vec::new();
        write_nar(&mut nar, &src).unwrap();

        let dst = base.join("dst");
        read_nar(&mut nar.as_slice(), &dst).unwrap();

        let mut nar2 = Vec::new();
        write_nar(&mut nar2, &dst).unwrap();
        assert_eq!(nar, nar2, "restored tree did not re-serialize identically");

        // Durable behavioral legs the byte-compare also implies, asserted directly:
        assert_eq!(fs::read(dst.join("a")).unwrap(), b"plain\n");
        assert_eq!(fs::read(dst.join("sub").join("nested")).unwrap(), b"deep\n");
        assert_eq!(fs::read_link(dst.join("lnk")).unwrap(), Path::new("a"));
        let run_mode = fs::symlink_metadata(dst.join("run")).unwrap().permissions().mode();
        assert!(run_mode & 0o100 != 0, "executable bit lost on restore");
        let a_mode = fs::symlink_metadata(dst.join("a")).unwrap().permissions().mode();
        assert!(a_mode & 0o100 == 0, "plain file restored executable");

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_rejects_a_truncated_archive() {
        // A corrupted/short download must error, never restore a partial tree — so the
        // consumer can fall back to building. (Self-discrimination: the read is strict.)
        let base = std::env::temp_dir().join(format!("td-nar-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        build_tree(&src);
        let mut nar = Vec::new();
        write_nar(&mut nar, &src).unwrap();
        nar.truncate(nar.len() - 24); // cut mid-stream

        let dst = base.join("dst");
        assert!(
            read_nar(&mut nar.as_slice(), &dst).is_err(),
            "read_nar accepted a truncated NAR"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_rejects_bad_magic() {
        // A fully valid NAR whose ONLY defect is the magic token — so the magic check is
        // the only thing that can reject it (the body would otherwise restore fine).
        let base = std::env::temp_dir().join(format!("td-nar-magic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("x"), b"hi\n").unwrap();
        let mut nar = Vec::new();
        write_nar(&mut nar, &src).unwrap();
        // "nix-archive-1" sits at bytes [8..21]; flip the trailing '1' to '2'.
        assert_eq!(&nar[8..21], b"nix-archive-1");
        nar[20] = b'2';
        assert!(read_nar(&mut nar.as_slice(), &base.join("dst")).is_err());
        fs::remove_dir_all(&base).unwrap();
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
