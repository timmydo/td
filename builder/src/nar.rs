//! NAR (Nix ARchive) serialization, bit-for-bit compatible with the pinned
//! daemon / (guix serialization) write-file — the S2 oracle semantics, read
//! off the pin:
//!   - tokens and contents are framed as u64 little-endian length + bytes,
//!     zero-padded to the next 8-byte boundary;
//!   - directory entries are sorted in codepoint order ("." and ".." never
//!     appear: read_dir does not yield them);
//!   - a regular file is "executable" iff (mode & 0o100);
//!   - symlink targets are written verbatim (readlink, no resolution).
//! The serialization streams into any Write — the nar-hash CLI wires it to
//! the SHA-256 hasher so file contents are never buffered whole.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
#![allow(unsafe_code)] // confined raw-syscall / low-level layer (UNSAFE.md)

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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

/// `O_NOFOLLOW` on Linux/x86-64 (asm-generic `fcntl.h`), spelled here because the engine
/// is `libc`-free. Only ever ORed into an `O_CREAT|O_EXCL` open, which already refuses an
/// existing symlink — so a wrong value would cost the belt, never the braces. It is
/// nonetheless pinned behaviorally by a test, since a bit the kernel merely ignored is
/// indistinguishable from the belt working.
pub(crate) const O_NOFOLLOW: i32 = 0o400_000;

/// Cap on directory nesting. Without it the only bound on the reader's recursion is
/// `PATH_MAX` failing a `create_dir` around 1300 levels down — arithmetic over two
/// unrelated constants rather than a stated limit, and a stack overflow is not a failure
/// this crate can return. No real store path is near this.
const MAX_NAR_DEPTH: u32 = 512;

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

/// Create a regular file at PATH, refusing an existing path rather than writing through
/// it. `O_EXCL` refuses one of any type, a symlink included wherever it points, and
/// `O_NOFOLLOW` says so independently of that.
fn create_regular(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
}

/// Restore one node at PATH. Sets CREATED once PATH itself exists, which is what tells
/// `read_nar` whether a failure left work of its own to remove; DEPTH counts directory
/// nesting against `MAX_NAR_DEPTH`.
fn read_node(
    input: &mut impl Read,
    path: &Path,
    created: &mut bool,
    depth: u32,
) -> io::Result<()> {
    if depth > MAX_NAR_DEPTH {
        return Err(invalid(format!("NAR nested deeper than {MAX_NAR_DEPTH}")));
    }
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
            let mut f = create_regular(path)?;
            *created = true;
            copy_n(input, &mut f, len)?;
            read_padding(input, len)?;
            // Restore only what NAR encodes: the executable bit (mode & 0o100). Through
            // the DESCRIPTOR (fchmod) rather than the path: resolving the name a second
            // time would follow a symlink swapped in since the create and hand the mode
            // to whatever it points at — undoing the fail-closed open three lines up.
            let mode = if exec { 0o755 } else { 0o644 };
            f.set_permissions(fs::Permissions::from_mode(mode))?;
            expect(input, ")")
        }
        b"symlink" => {
            expect(input, "target")?;
            let target = read_framed(input)?;
            // Same argument as an entry name's NUL: `symlink(2)` refuses both of these,
            // but only a check here says which archive and which field.
            if target.is_empty() || target.contains(&0) {
                return Err(invalid("unsafe NAR symlink target"));
            }
            // OsStr round-trip keeps a non-UTF-8 target intact (mirror of write_node).
            let target = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&target) };
            std::os::unix::fs::symlink(target, path)?;
            *created = true;
            expect(input, ")")
        }
        b"directory" => {
            fs::create_dir(path)?;
            *created = true;
            let mut prev: Option<Vec<u8>> = None;
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
                // NUL is rejected HERE rather than left to `CString`'s conversion deep in
                // `std`: it fails either way, but only this one names the archive.
                if name.is_empty()
                    || name == b"."
                    || name == b".."
                    || name.contains(&b'/')
                    || name.contains(&0)
                {
                    return Err(invalid("unsafe NAR entry name"));
                }
                // Strictly increasing, as the writer emits them and as Nix's reader
                // requires. Two entries of one name are two nodes at one path — the
                // second lands on whatever the first left there — and a NAR out of
                // order is not canonical, so it would re-serialize to different bytes
                // and a different hash than the one that admitted it.
                if let Some(p) = &prev {
                    if name.as_slice() <= p.as_slice() {
                        return Err(invalid(format!(
                            "NAR entry names not strictly increasing: {:?} after {:?}",
                            String::from_utf8_lossy(&name),
                            String::from_utf8_lossy(p)
                        )));
                    }
                }
                expect(input, "node")?;
                let child = path.join(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&name) });
                // A child's own flag: this node's CREATED is already true, and what the
                // caller needs to know is whether PATH exists, not how much is under it.
                let mut child_created = false;
                read_node(input, &child, &mut child_created, depth + 1)?;
                expect(input, ")")?;
                prev = Some(name);
            }
        }
        other => Err(invalid(format!(
            "unknown NAR node type {:?}",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// Remove PATH whatever it is, reporting what went wrong. `remove_dir_all` unlinks a stale
/// SYMLINK but returns `ENOTDIR` on a regular FILE, which is what a single-file NAR leaves
/// at its destination. An absent PATH is success: the caller wanted it gone.
pub fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The top-level node, plus the requirement that the stream END there. Trailing bytes are
/// the ordering check's own argument reached from the other side: an archive with anything
/// appended is not canonical, so it would not re-serialize to the bytes that arrived — and
/// the `nar-restore` CLI verifies no hash, so nothing else would notice.
fn read_top(input: &mut impl Read, dest: &Path, created: &mut bool) -> io::Result<()> {
    read_node(input, dest, created, 0)?;
    let mut extra = [0u8; 1];
    if input.read(&mut extra)? != 0 {
        return Err(invalid("trailing bytes after the NAR's top-level node"));
    }
    Ok(())
}

/// Restore a NAR stream from INPUT onto DEST (which must not already exist). The inverse
/// of `write_nar`: `write_nar(.., p)` then `read_nar(.., q)` reconstructs the tree at `p`
/// under `q` (same contents, executable bits, symlink targets, directory structure).
///
/// A failure leaves NOTHING at DEST. That is the "never a partial tree" promise above, and
/// it belongs here rather than in each caller: entries created before the failing one are
/// reached by construction — an out-of-order or truncated archive fails part way through —
/// and until now only the consumer that remembered to clean up got it.
pub fn read_nar(input: &mut impl Read, dest: &Path) -> io::Result<()> {
    if read_framed(input)? != b"nix-archive-1" {
        return Err(invalid("not a NAR (bad magic)"));
    }
    // Enforced rather than merely documented, so that a refusal is reported as one instead
    // of arriving as a bare EEXIST from whichever create happened to run first.
    if fs::symlink_metadata(dest).is_ok() {
        return Err(invalid(format!(
            "restore destination already exists: {}",
            dest.display()
        )));
    }
    let mut created = false;
    match read_top(input, dest, &mut created) {
        Ok(()) => Ok(()),
        // Gated on having CREATED dest, not on the error kind. Either side of that is a
        // way to be wrong: cleaning up unconditionally would delete a path this call never
        // made — the check above passed and something else won the race — while keying off
        // `AlreadyExists` would skip cleanup for a collision deeper in a tree that IS ours,
        // leaving the partial tree the doc above promises never to leave, at a destination
        // the refusal above then makes permanently unrestorable.
        Err(e) if !created => Err(e),
        // Both failures, td-init's mknod readback's rule: a cleanup that silently failed
        // would leave the caller believing DEST is clean and no diagnostic saying why every
        // later restore of it is refused.
        Err(e) => Err(match remove_any(dest) {
            Ok(()) => e,
            Err(c) => io::Error::new(
                e.kind(),
                format!("{e}; and {} could not be removed: {c}", dest.display()),
            ),
        }),
    }
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
        assert!(
            fs::symlink_metadata(&dst).is_err(),
            "a truncated NAR left its partial tree at dest"
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

    // ---- hostile archives: hand-built, because write_nar cannot emit one ----
    // The writer sorts and de-duplicates by construction (read_dir yields each name
    // once), so every leg below has to lay the bytes out itself.

    fn node_regular(contents: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for t in ["(", "type", "regular", "contents"] {
            v.extend(framed(t.as_bytes()));
        }
        v.extend(framed(contents));
        v.extend(framed(b")"));
        v
    }

    fn node_symlink(target: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for t in ["(", "type", "symlink", "target"] {
            v.extend(framed(t.as_bytes()));
        }
        v.extend(framed(target));
        v.extend(framed(b")"));
        v
    }

    /// A whole NAR of a directory holding ENTRIES verbatim, in the order given.
    fn nar_of_dir(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
        let mut v = framed(b"nix-archive-1");
        for t in ["(", "type", "directory"] {
            v.extend(framed(t.as_bytes()));
        }
        for (name, node) in entries {
            for t in ["entry", "(", "name"] {
                v.extend(framed(t.as_bytes()));
            }
            v.extend(framed(name));
            v.extend(framed(b"node"));
            v.extend(node.clone());
            v.extend(framed(b")"));
        }
        v.extend(framed(b")"));
        v
    }

    #[test]
    fn read_nar_refuses_to_write_through_a_symlink_entry() {
        // Two entries of ONE name, symlink first: a create that follows the link writes
        // the second entry's contents wherever the first one points — outside dest, with
        // Ok(()) returned and nothing under dest to show for it.
        let base = std::env::temp_dir().join(format!("td-nar-dup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let outside = base.join("outside");
        fs::write(&outside, b"original\n").unwrap();

        let target = outside.as_os_str().as_encoded_bytes().to_vec();
        let nar = nar_of_dir(&[
            (b"x", node_symlink(&target)),
            (b"x", node_regular(b"pwned\n")),
        ]);
        let dst = base.join("dst");
        let r = read_nar(&mut nar.as_slice(), &dst);
        assert!(r.is_err(), "read_nar accepted two entries of one name");
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"original\n",
            "read_nar wrote through a symlink entry, outside dest"
        );
        // The failure lands AFTER the first entry was created, so this is where the
        // "never a partial tree" promise is tested: the hostile symlink must not survive
        // as residue for a retry or a fallback to meet.
        assert!(
            fs::symlink_metadata(&dst).is_err(),
            "a rejected NAR left its partial tree at dest"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_requires_strictly_increasing_entry_names() {
        // Both legs are well-formed archives whose ONLY defect is entry ORDER, so the
        // ordering check is the only thing that can reject them. Equal names are the
        // symlink case above with the follow closed; descending names are the same
        // archive re-ordered, and a NAR is canonical or it is not one.
        let base = std::env::temp_dir().join(format!("td-nar-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        for (leg, entries) in [
            ("descending", vec![(&b"b"[..], node_regular(b"2")), (&b"a"[..], node_regular(b"1"))]),
            ("equal", vec![(&b"a"[..], node_regular(b"1")), (&b"a"[..], node_regular(b"2"))]),
        ] {
            let dst = base.join(leg);
            assert!(
                read_nar(&mut nar_of_dir(&entries).as_slice(), &dst).is_err(),
                "read_nar accepted {leg} entry names"
            );
            assert!(
                fs::symlink_metadata(&dst).is_err(),
                "{leg}: a rejected NAR left its partial tree at dest"
            );
        }
        // The same entries in increasing order are accepted — so the leg above rejects
        // the ORDER and not the archive.
        let ok = nar_of_dir(&[(&b"a"[..], node_regular(b"1")), (&b"b"[..], node_regular(b"2"))]);
        let dst = base.join("sorted");
        read_nar(&mut ok.as_slice(), &dst).unwrap();
        assert_eq!(fs::read(dst.join("b")).unwrap(), b"2");
        fs::remove_dir_all(&base).unwrap();
    }

    /// `ELOOP` on Linux/x86-64 (asm-generic `errno.h`), spelled out for `O_NOFOLLOW`'s own
    /// reason: the assertion below turns entirely on this number, and an errno the open can
    /// return anyway (ENOENT, EACCES) would pass while proving nothing about the flag.
    const ELOOP: i32 = 40;

    #[test]
    fn create_regular_refuses_an_existing_path_rather_than_writing_through_it() {
        // The create-side belt on its own. The ordering check rejects a duplicate-name
        // archive BEFORE the second create is reached, so no archive-level test can pin
        // this — and the case it exists for is a path planted between the check and the
        // create, at any depth, which no archive controls either.
        let base = std::env::temp_dir().join(format!("td-nar-create-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let outside = base.join("outside");
        fs::write(&outside, b"original\n").unwrap();

        let link = base.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(create_regular(&link).is_err(), "created through a symlink");
        assert_eq!(fs::read(&outside).unwrap(), b"original\n", "wrote through a symlink");

        let file = base.join("file");
        fs::write(&file, b"mine\n").unwrap();
        assert!(create_regular(&file).is_err(), "created over an existing file");
        assert_eq!(fs::read(&file).unwrap(), b"mine\n", "truncated an existing file");

        // And it does create where nothing is, so the refusals above are the belt rather
        // than the call being broken.
        create_regular(&base.join("fresh")).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn nofollow_is_the_value_the_kernel_reads_as_o_nofollow() {
        // Pins the hand-spelled constant BEHAVIORALLY (the engine is libc-free, so there
        // is no header to agree with). Without `create_new` the flag is the only thing
        // that can refuse a symlink, so ELOOP is what says the kernel read it as
        // O_NOFOLLOW rather than as an ignored bit.
        let base = std::env::temp_dir().join(format!("td-nar-nofollow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let target = base.join("target");
        fs::write(&target, b"t").unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let e = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(O_NOFOLLOW)
            .open(&link)
            .expect_err("O_NOFOLLOW did not refuse a symlink");
        assert_eq!(e.raw_os_error(), Some(ELOOP), "expected ELOOP, got {e:?}");
        // Without the flag the same open follows the link — so the leg above is the flag
        // and not something about the path.
        fs::OpenOptions::new().write(true).create(true).truncate(false).open(&link).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_rejects_an_entry_name_holding_a_nul() {
        // Reachable: a name is arbitrary framed bytes. It fails either way (std's CString
        // conversion refuses it), but the reader's own diagnostic is what names the cause.
        let base = std::env::temp_dir().join(format!("td-nar-nul-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let nar = nar_of_dir(&[(&b"a\0b"[..], node_regular(b"1"))]);
        let e = read_nar(&mut nar.as_slice(), &base.join("dst"))
            .expect_err("read_nar accepted a NUL in an entry name");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("unsafe NAR entry name"), "{e}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_rejects_trailing_bytes_after_the_top_node() {
        // A complete, otherwise valid archive with data appended: not canonical, so it
        // would not re-serialize to the bytes it arrived as — and the CLI checks no hash,
        // so this is the only thing that would notice.
        let base = std::env::temp_dir().join(format!("td-nar-trail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        build_tree(&src);
        let mut nar = Vec::new();
        write_nar(&mut nar, &src).unwrap();
        nar.extend_from_slice(b"\0\0\0\0\0\0\0\0");

        let dst = base.join("dst");
        assert!(read_nar(&mut nar.as_slice(), &dst).is_err(), "read_nar accepted trailing bytes");
        assert!(
            fs::symlink_metadata(&dst).is_err(),
            "a rejected NAR left its tree at dest"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_rejects_a_directory_nested_past_the_depth_cap() {
        // The bound is stated rather than inherited from PATH_MAX, so it is reachable with
        // one-byte names and a few KiB of archive.
        let base = std::env::temp_dir().join(format!("td-nar-deep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        // Innermost first: a directory node wrapping N-1 more of itself.
        let levels = MAX_NAR_DEPTH as usize + 2;
        let mut node = Vec::new();
        for t in ["(", "type", "directory", ")"] {
            node.extend(framed(t.as_bytes()));
        }
        for _ in 0..levels {
            let mut outer = Vec::new();
            for t in ["(", "type", "directory", "entry", "(", "name"] {
                outer.extend(framed(t.as_bytes()));
            }
            outer.extend(framed(b"d"));
            outer.extend(framed(b"node"));
            outer.extend(node);
            for t in [")", ")"] {
                outer.extend(framed(t.as_bytes()));
            }
            node = outer;
        }
        let mut nar = framed(b"nix-archive-1");
        nar.extend(node);

        let dst = base.join("dst");
        let e = read_nar(&mut nar.as_slice(), &dst).expect_err("read_nar accepted unbounded nesting");
        assert!(e.to_string().contains("nested deeper than"), "{e}");
        assert!(fs::symlink_metadata(&dst).is_err(), "a rejected NAR left its tree at dest");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_rejects_a_symlink_target_that_is_empty_or_holds_a_nul() {
        // The field whose archive bytes reach a syscall least filtered. Both fail at
        // `symlink(2)` anyway; what the check buys is a diagnostic naming the archive.
        let base = std::env::temp_dir().join(format!("td-nar-tgt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        for (leg, target) in [("empty", &b""[..]), ("nul", &b"a\0b"[..])] {
            let mut nar = framed(b"nix-archive-1");
            nar.extend(node_symlink(target));
            let e = read_nar(&mut nar.as_slice(), &base.join(leg))
                .expect_err("read_nar accepted a bad symlink target")
                .to_string();
            assert!(e.contains("unsafe NAR symlink target"), "{leg}: {e}");
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn read_nar_refuses_a_dest_that_already_exists() {
        // dest is documented as "must not already exist" and nothing enforced it: a
        // top-level regular node created through an existing symlink is the same
        // arbitrary write, reached from the nar-restore CLI's own argv.
        let base = std::env::temp_dir().join(format!("td-nar-dest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let outside = base.join("outside");
        fs::write(&outside, b"original\n").unwrap();
        let dst = base.join("dst");
        std::os::unix::fs::symlink(&outside, &dst).unwrap();

        let mut nar = framed(b"nix-archive-1");
        nar.extend(node_regular(b"pwned\n"));
        assert!(read_nar(&mut nar.as_slice(), &dst).is_err(), "read_nar accepted an existing dest");
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"original\n",
            "read_nar wrote through a symlink at dest"
        );
        // The converse of the cleanup: a destination read_nar REFUSED is not its to
        // remove. Deleting on the way out of this arm would turn a refusal into the
        // destructive act the refusal exists to prevent.
        assert!(
            fs::symlink_metadata(&dst).is_ok(),
            "read_nar deleted a pre-existing dest it had refused"
        );
        fs::remove_dir_all(&base).unwrap();
    }
}
