use std::io::{BufReader, Cursor};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};

pub fn extract_tar(tar: &Path, dest: &Path) -> Result<(), String> {
    let mut file = File::open(tar).map_err(|e| format!("open {}: {e}", tar.display()))?;
    extract_tar_reader(&mut file, &tar.display().to_string(), dest)
}

pub fn extract_tar_gz(tar_gz: &Path, dest: &Path) -> Result<(), String> {
    let bytes = crate::gzip::decompress_file(tar_gz)?;
    let mut reader = Cursor::new(bytes);
    extract_tar_reader(&mut reader, &tar_gz.display().to_string(), dest)
}

pub fn extract_tar_bz2(tar_bz2: &Path, dest: &Path) -> Result<(), String> {
    let bytes = crate::bzip2::decompress_file(tar_bz2)?;
    let mut reader = Cursor::new(bytes);
    extract_tar_reader(&mut reader, &tar_bz2.display().to_string(), dest)
}

fn remove_temp_path(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("stat temporary tree {}: {e}", path.display())),
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).map_err(|e| format!("clear {}: {e}", path.display()))
    } else {
        fs::remove_file(path).map_err(|e| format!("clear {}: {e}", path.display()))
    }
}

/// Best-effort cleanup on every error path. Successful paths remove the tree
/// explicitly so a cleanup failure is still returned to the caller.
struct TempTree(PathBuf);

static TEMP_TREE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl TempTree {
    fn create_sibling(dest: &Path, suffix: &str) -> Result<Self, String> {
        let name = dest.file_name().ok_or_else(|| {
            format!(
                "destination {} has no file name to place a temporary tree beside",
                dest.display()
            )
        })?;
        if let Some(parent) = dest
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "create destination parent {} for temporary extraction: {e}",
                    parent.display()
                )
            })?;
        }
        for _ in 0..128 {
            let sequence = TEMP_TREE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = name.to_os_string();
            temp_name.push(format!("{suffix}-{}-{sequence}", std::process::id()));
            let path = dest.with_file_name(temp_name);
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(format!(
                        "create temporary tree beside {}: {e}",
                        dest.display()
                    ))
                }
            }
        }
        Err(format!(
            "cannot allocate a unique temporary tree beside {} after 128 attempts",
            dest.display()
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = remove_temp_path(&self.0);
    }
}

fn finish_xz_tar(
    extracted: Result<(), String>,
    decoded: Result<(), String>,
    drained: Result<(), String>,
) -> Result<(), String> {
    match (extracted, decoded) {
        (Err(tar), Err(xz)) if xz.contains("tar extractor stopped") => Err(tar),
        (_, Err(xz)) => Err(xz),
        (Err(tar), Ok(())) => Err(tar),
        (Ok(()), Ok(())) => drained,
    }
}

pub fn extract_tar_xz(tar_xz: &Path, dest: &Path) -> Result<(), String> {
    // Tar entries arrive before the XZ block check/index/footer. Keep them in
    // a disposable sibling until the decoder validates the COMPLETE stream;
    // otherwise a corrupt fixed-output source can leave an apparently usable
    // partial tree behind even though extraction returned Err.
    let verified = TempTree::create_sibling(dest, ".xz-verify-tmp")?;
    let file = File::open(tar_xz).map_err(|e| format!("open {}: {e}", tar_xz.display()))?;
    let source = tar_xz.display().to_string();
    // The decoder and extractor run concurrently over a two-chunk bounded
    // channel. XZ retains only its declared dictionary; tar retains only its
    // current entry. Drain after tar's zero block so the decoder can validate
    // the XZ index/footer/check without blocking on trailing tar padding.
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(2);
        let decoder = std::thread::Builder::new()
            .name("td-xz-decoder".to_string())
            .spawn_scoped(scope, move || {
                let mut input = BufReader::new(file);
                let mut writer = XzChunkWriter { sender };
                crate::xz::decompress_reader_to(&mut input, &mut writer).map(|_| ())
            })
            .map_err(|e| format!("spawn xz decoder for {source}: {e}"))?;
        let mut reader = XzChunkReader {
            receiver,
            chunk: Vec::new(),
            offset: 0,
        };
        let extracted = extract_tar_reader(&mut reader, &source, verified.path());
        let drained = if extracted.is_ok() {
            io::copy(&mut reader, &mut io::sink())
                .map(|_| ())
                .map_err(|e| format!("drain decompressed xz stream from {source}: {e}"))
        } else {
            Ok(())
        };
        // On a tar error this closes the channel immediately, making the
        // decoder stop at its next bounded write instead of burning through a
        // multi-gigabyte remainder. Preserve that first, actionable tar error.
        drop(reader);
        let decoded = match decoder.join() {
            Ok(result) => result,
            Err(_) => Err(format!("xz decoder thread panicked for {source}")),
        };
        finish_xz_tar(extracted, decoded, drained)
    })?;
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    merge_move(verified.path(), dest)?;
    remove_temp_path(verified.path())
}

struct XzChunkWriter {
    sender: SyncSender<Vec<u8>>,
}

impl Write for XzChunkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sender
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tar extractor stopped"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct XzChunkReader {
    receiver: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    offset: usize,
}

impl Read for XzChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.offset >= self.chunk.len() {
            match self.receiver.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let available = self.chunk.get(self.offset..).unwrap_or(&[]);
        let n = available.len().min(buf.len());
        let Some(src) = available.get(..n) else {
            return Ok(0);
        };
        let Some(dst) = buf.get_mut(..n) else {
            return Ok(0);
        };
        dst.copy_from_slice(src);
        self.offset += n;
        Ok(n)
    }
}

/// Unpack a tarball by MAGIC BYTES (gzip 1f8b, bzip2 "BZh", xz fd377a585a00;
/// anything else is read as plain tar) — the engine-native `unpack` step's
/// entry, so no rung declares tar/gzip/bzip2/xz packages just to open its
/// source (re #469). `keep_top: false` = `tar --strip-components=1`: the
/// archive must have a UNIQUE top-level directory, which is elided; multiple
/// top-level entries under strip is a hard error, never a silent mangle.
pub fn unpack_archive(tarball: &Path, dest: &Path, keep_top: bool) -> Result<(), String> {
    let extract_into = |d: &Path| -> Result<(), String> {
        let mut f = File::open(tarball).map_err(|e| format!("open {}: {e}", tarball.display()))?;
        let mut magic = [0u8; 6];
        let n = f
            .read(&mut magic)
            .map_err(|e| format!("read {}: {e}", tarball.display()))?;
        match magic.get(..n).unwrap_or(&[]) {
            [0x1f, 0x8b, ..] => extract_tar_gz(tarball, d),
            [b'B', b'Z', b'h', ..] => extract_tar_bz2(tarball, d),
            [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => extract_tar_xz(tarball, d),
            _ => extract_tar(tarball, d),
        }
    };
    if keep_top {
        return extract_into(dest);
    }
    // Strip the top-level dir: extract beside dest (same filesystem, so the
    // child renames below are atomic moves), then hoist the unique top dir's
    // children into dest. The tmp name APPENDS to dest's file name —
    // `with_extension` would replace an existing extension, letting two
    // distinct dests collide on one tmp path (and the pre-clean below would
    // then delete the other unpack's tree).
    let tmp = TempTree::create_sibling(dest, ".unpack-tmp")
        .map_err(|e| format!("unpack {}: {e}", tarball.display()))?;
    extract_into(tmp.path())?;
    let mut tops = fs::read_dir(tmp.path())
        .map_err(|e| format!("read {}: {e}", tmp.path().display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read {}: {e}", tmp.path().display()))?;
    // The sole top entry must be a REAL directory — `file_type()` does not
    // follow symlinks. A symlink-to-dir top would make the hoist read and
    // rename through the link's target, moving files out of a tree OUTSIDE
    // the extracted archive.
    let top = match (tops.pop(), tops.is_empty()) {
        (Some(t), true) if t.file_type().is_ok_and(|ft| ft.is_dir()) => t.path(),
        _ => {
            return Err(format!(
                "unpack {}: stripping the top level needs exactly one top-level directory",
                tarball.display()
            ))
        }
    };
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    merge_move(&top, dest)?;
    remove_temp_path(tmp.path())
}

/// Move FROM's children into DEST, MERGING into existing directories (the
/// `tar --strip-components=1` overlay semantics some rungs rely on — e.g. a
/// g++ add-on tarball unpacked over its gcc tree). Files/symlinks rename over
/// an existing file; a file/directory kind clash is a hard error.
fn merge_move(from: &Path, dest: &Path) -> Result<(), String> {
    for ent in fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))? {
        let ent = ent.map_err(|e| format!("read {}: {e}", from.display()))?;
        let src = ent.path();
        let to = dest.join(ent.file_name());
        let src_is_dir = ent
            .file_type()
            .map_err(|e| format!("stat {}: {e}", src.display()))?
            .is_dir();
        let to_meta = fs::symlink_metadata(&to);
        match (src_is_dir, to_meta) {
            // Nothing at the target: a plain move either way.
            (_, Err(_)) => fs::rename(&src, &to)
                .map_err(|e| format!("move {} -> {}: {e}", src.display(), to.display()))?,
            // Dir onto dir: recurse-merge.
            (true, Ok(m)) if m.is_dir() => merge_move(&src, &to)?,
            // File/symlink onto file/symlink: replace (rename semantics).
            (false, Ok(m)) if !m.is_dir() => fs::rename(&src, &to)
                .map_err(|e| format!("move {} -> {}: {e}", src.display(), to.display()))?,
            _ => {
                return Err(format!(
                    "unpack merge: {} and {} disagree on file vs directory",
                    src.display(),
                    to.display()
                ))
            }
        }
    }
    Ok(())
}

include!("tar_extract.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xz_root_cause_wins_unless_tar_closed_the_channel_first() {
        assert_eq!(
            finish_xz_tar(
                Err("tar EOF".to_string()),
                Err("xz CRC64 mismatch".to_string()),
                Ok(()),
            ),
            Err("xz CRC64 mismatch".to_string())
        );
        assert_eq!(
            finish_xz_tar(
                Err("escaping tar path".to_string()),
                Err("write decompressed xz output: tar extractor stopped".to_string()),
                Ok(()),
            ),
            Err("escaping tar path".to_string())
        );
    }

    #[test]
    fn temporary_extraction_creates_a_missing_destination_parent() {
        let tmp = temp_dir("td-tar-temp-parent");
        let dest = tmp.join("missing/nested/output");
        let tree = TempTree::create_sibling(&dest, ".test-tmp").unwrap();
        let temporary = tree.path().to_path_buf();
        assert!(dest.parent().unwrap().is_dir());
        assert!(temporary.is_dir());
        drop(tree);
        assert!(!temporary.exists());
    }

    #[test]
    fn corrupt_xz_never_publishes_unverified_tar_entries() {
        // `visible`, an empty ustar file, compressed with XZ Utils 5.4.5
        // (`xz -0 --check=crc64`). Byte 124 is in the block CRC64, after the
        // complete tar stream, so extraction has created the entry before the
        // decoder discovers the corruption.
        let mut archive = [
            0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46, 0x02, 0x00,
            0x21, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x8f, 0x98, 0x41, 0x9c, 0xe0, 0x27, 0xff, 0x00,
            0x5c, 0x5d, 0x00, 0x3b, 0x1a, 0x4a, 0xcd, 0x37, 0xbe, 0xc0, 0x61, 0xb2, 0x3a, 0x7a,
            0xf5, 0x32, 0xe0, 0xa9, 0x81, 0xd3, 0x13, 0xd9, 0x3e, 0x5f, 0xc4, 0x81, 0x95, 0xb8,
            0xa4, 0x8c, 0xc1, 0xfb, 0x85, 0x1d, 0xe8, 0xbc, 0x8f, 0xbc, 0x0e, 0xf7, 0x81, 0x50,
            0xa0, 0xf2, 0x60, 0xdd, 0xd1, 0xbb, 0x8c, 0x92, 0xa9, 0x86, 0x5d, 0x72, 0x0f, 0xd1,
            0x80, 0x64, 0x10, 0xe1, 0xc5, 0xc8, 0x47, 0x0e, 0x1a, 0x06, 0xe5, 0xcf, 0xec, 0xbc,
            0x6e, 0x7e, 0x11, 0xfb, 0xfe, 0xda, 0x5b, 0xd8, 0x0c, 0x88, 0xf8, 0xe7, 0x30, 0x79,
            0x09, 0xc6, 0x2b, 0x6f, 0xa9, 0x23, 0x4e, 0xfd, 0xd1, 0x7a, 0x00, 0x00, 0xcd, 0xa5,
            0x88, 0x7d, 0xee, 0x27, 0x1e, 0xeb, 0x00, 0x01, 0x78, 0x80, 0x50, 0x00, 0x00, 0x00,
            0x41, 0xe4, 0xcf, 0x5e, 0xb1, 0xc4, 0x67, 0xfb, 0x02, 0x00, 0x00, 0x00, 0x00, 0x04,
            0x59, 0x5a,
        ];
        let Some(check_byte) = archive.get_mut(124) else {
            panic!("fixture must contain its block check");
        };
        *check_byte ^= 0xff;
        let tmp = temp_dir("td-tar-corrupt-xz");
        let dest = tmp.join("published");
        // This is the old predictable temporary name. It is deliberately the
        // INPUT: allocation must skip any pre-existing path, never pre-clean it.
        let source = tmp.join("published.xz-verify-tmp");
        fs::write(&source, archive).unwrap();

        let err = extract_tar_xz(&source, &dest).expect_err("bad XZ check must red");

        assert!(err.contains("CRC64 mismatch"), "got: {err}");
        assert!(
            source.is_file(),
            "temporary allocation deleted the input archive"
        );
        assert!(
            !dest.exists(),
            "unverified destination must not be published"
        );
        let leaked_temp = fs::read_dir(&tmp).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("published.xz-verify-tmp-")
        });
        assert!(!leaked_temp, "unverified temporary tree must be removed");
    }

    #[test]
    fn refuses_parent_components() {
        let err = safe_join(Path::new("/tmp/root"), Path::new("../x")).expect_err("unsafe path");
        assert!(err.contains("escaping tar path"));
    }

    #[test]
    fn unpack_archive_keep_top_round_trips_an_uncompressed_tar() {
        // Plain tar sniffing must preserve the top directory when requested.
        let tmp = temp_dir("td-tar-keeptop");
        let tar = tmp.join("plain.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "kh/asm/types.h", b'0', 0o644, 5, "");
        append_data(&mut bytes, b"hello");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        unpack_archive(&tar, &out, true).unwrap();

        // Top-level "kh" dir preserved (no --strip-components), content intact.
        assert_eq!(fs::read(out.join("kh/asm/types.h")).unwrap(), b"hello");
    }

    #[test]
    fn extracts_gnu_long_name_hardlinks_and_absolute_symlinks() {
        let tmp = temp_dir("td-tar-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let long_target = format!(
            "gnu/store/{}-target/bin/very-long-file-name",
            "a".repeat(90)
        );
        let long_link = format!("gnu/store/{}-link/bin/very-long-file-name", "b".repeat(90));
        let mut bytes = Vec::new();
        append_long_name(&mut bytes, &long_target);
        append_header(&mut bytes, "short-target", b'0', 0o644, 5, "");
        append_data(&mut bytes, b"hello");
        append_long_link(&mut bytes, &long_target);
        append_long_name(&mut bytes, &long_link);
        append_header(&mut bytes, "short-link", b'1', 0o644, 0, "short-target");
        append_header(
            &mut bytes,
            "gnu/store/symlink",
            b'2',
            0o777,
            0,
            "/gnu/store/absolute-target",
        );
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        assert_eq!(fs::read(out.join(&long_target)).unwrap(), b"hello");
        assert_eq!(fs::read(out.join(&long_link)).unwrap(), b"hello");
        assert_eq!(
            fs::read_link(out.join("gnu/store/symlink")).unwrap(),
            PathBuf::from("/gnu/store/absolute-target")
        );
    }

    #[test]
    fn refuses_bad_header_checksum() {
        let tmp = temp_dir("td-tar-checksum-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "file", b'0', 0o644, 5, "");
        append_data(&mut bytes, b"hello");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        let first = bytes.get_mut(0).unwrap();
        *first = b'F';
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("corrupt header should fail");

        assert!(err.contains("checksum mismatch"));
        assert!(!out.join("File").exists());
    }

    #[test]
    fn refuses_oversized_regular_entry_before_writing() {
        let tmp = temp_dir("td-tar-oversized-regular-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "huge", b'0', 0o644, MAX_TAR_ENTRY_BYTES + 1, "");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("oversized entry should fail");

        assert!(err.contains("too large"), "got: {err}");
        assert!(!out.join("huge").exists());
    }

    #[test]
    fn aggregate_archive_budget_rejects_entries_and_payload_before_accounting() {
        let entry = Entry {
            path: PathBuf::from("small"),
            size: 1,
            mode: 0o644,
            mtime: None,
            kind: EntryKind::Regular,
        };
        let mut entries = ArchiveBudget {
            entries: MAX_TAR_ENTRIES,
            payload: 0,
        };
        assert!(entries.account(&entry).is_err());
        assert_eq!(entries.entries, MAX_TAR_ENTRIES);
        assert_eq!(entries.payload, 0);

        let mut payload = ArchiveBudget {
            entries: 0,
            payload: MAX_TAR_PAYLOAD_BYTES,
        };
        assert!(payload.account(&entry).is_err());
        assert_eq!(payload.entries, 0);
        assert_eq!(payload.payload, MAX_TAR_PAYLOAD_BYTES);
    }

    #[test]
    fn refuses_empty_partial_and_single_marker_archives() {
        let tmp = temp_dir("td-tar-truncated-header-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");

        fs::write(&tar, []).unwrap();
        let empty = extract_tar(&tar, &out).expect_err("empty input is not a tar archive");
        assert!(empty.contains("read 0 of 512 bytes"), "got: {empty}");

        fs::write(&tar, [b'x'; 17]).unwrap();
        let partial = extract_tar(&tar, &out).expect_err("partial header must fail");
        assert!(partial.contains("read 17 of 512 bytes"), "got: {partial}");

        fs::write(&tar, [0u8; BLOCK]).unwrap();
        let marker = extract_tar(&tar, &out).expect_err("one zero marker must fail");
        assert!(marker.contains("read 0 of 512 bytes"), "got: {marker}");

        let mut trailing = vec![0u8; BLOCK * 2];
        trailing.extend_from_slice(b"not-zero-padding");
        fs::write(&tar, trailing).unwrap();
        let nonzero = extract_tar(&tar, &out).expect_err("nonzero trailing data must fail");
        assert!(nonzero.contains("nonzero data"), "got: {nonzero}");

        fs::write(&tar, vec![0u8; BLOCK * 3]).unwrap();
        extract_tar(&tar, &out).expect("additional zero padding is valid");
    }

    #[test]
    fn refuses_oversized_gnu_long_name_before_allocating() {
        let tmp = temp_dir("td-tar-oversized-long-name-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header(
            &mut bytes,
            "././@LongLink",
            b'L',
            0o644,
            MAX_GNU_LONG_FIELD_BYTES + 1,
            "",
        );
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("oversized long name should fail");

        assert!(err.contains("metadata-header field"), "got: {err}");
        assert!(err.contains("too large"), "got: {err}");
    }

    #[test]
    fn refuses_data_payload_on_links_and_directories() {
        let tmp = temp_dir("td-tar-link-size-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "link", b'2', 0o777, 1, "target");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("symlink payload should fail");

        assert!(err.contains("must not carry 1 data bytes"), "got: {err}");
        assert!(!out.join("link").exists());
    }

    #[test]
    fn preserves_regular_file_mtime() {
        let tmp = temp_dir("td-tar-mtime-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mtime = 1_234_567_890u64; // 2009-02-13
        let mut bytes = Vec::new();
        append_header_mtime(&mut bytes, "stamped", b'0', 0o644, 5, "", mtime);
        append_data(&mut bytes, b"hello");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        let secs = fs::metadata(out.join("stamped"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, mtime);
    }

    #[test]
    fn preserves_mtime_order_regardless_of_extraction_order() {
        // The exact gcc-2.95.3 shape: the generated bison parser `c-parse.c`
        // ships in the tarball BEFORE its source `c-parse.y`, but with a NEWER
        // mtime. GNU tar/Guix preserve that so `make` sees the parser as
        // up-to-date and never runs bison. If the unpacker stamped "now" in
        // extraction order the parser (written first) would end up OLDER than
        // its source and force a spurious bison run.
        let tmp = temp_dir("td-tar-mtime-order-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header_mtime(&mut bytes, "c-parse.c", b'0', 0o644, 3, "", 2_000);
        append_data(&mut bytes, b"gen");
        append_header_mtime(&mut bytes, "c-parse.y", b'0', 0o644, 3, "", 1_000);
        append_data(&mut bytes, b"src");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        let generated = fs::metadata(out.join("c-parse.c"))
            .unwrap()
            .modified()
            .unwrap();
        let source = fs::metadata(out.join("c-parse.y"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            generated > source,
            "generated parser must stay newer than its source"
        );
    }

    #[test]
    fn preserves_mtime_even_on_read_only_file() {
        // The mtime is set through the still-open fd BEFORE chmod drops
        // owner-write, so a 0o444 file still gets its archived mtime (futimens on
        // our own fd is unaffected by the file's mode). This exercises the exact
        // rationale the extraction code comments on.
        let tmp = temp_dir("td-tar-readonly-mtime-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mtime = 1_111_111_111u64;
        let mut bytes = Vec::new();
        append_header_mtime(&mut bytes, "ro", b'0', 0o444, 5, "", mtime);
        append_data(&mut bytes, b"hello");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        let meta = fs::metadata(out.join("ro")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o444);
        let secs = meta
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, mtime);
    }

    #[test]
    fn base256_mtime_falls_back_without_failing_extraction() {
        // A GNU base-256 mtime (high bit set in the field) is not octal; parse_octal
        // rejects it and Entry::parse falls back to None. Extraction must still
        // SUCCEED — the file keeps its extraction-time mtime — rather than error.
        let tmp = temp_dir("td-tar-base256-mtime-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut header = [0u8; BLOCK];
        write_bytes(&mut header, 0, 100, b"base256");
        write_octal(&mut header, 100, 8, 0o644);
        write_octal(&mut header, 108, 8, 0);
        write_octal(&mut header, 116, 8, 0);
        write_octal(&mut header, 124, 12, 5);
        // base-256 mtime: high-bit marker + magnitude bytes, deliberately NOT octal.
        header[136] = 0x80;
        header[147] = 0x2a;
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        header[156] = b'0';
        write_bytes(&mut header, 257, 6, b"ustar");
        write_bytes(&mut header, 263, 2, b"00");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        write_checksum(&mut header, sum);
        let mut bytes = header.to_vec();
        append_data(&mut bytes, b"hello");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        assert_eq!(fs::read(out.join("base256")).unwrap(), b"hello");
    }

    #[test]
    fn strip_top_unpack_merges_an_overlay_tree() {
        let tmp = temp_dir("td-tar-overlay-test");
        let dest = tmp.join("src");
        let core = tmp.join("core.tar");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "core-1.0/", b'5', 0o755, 0, "");
        append_header(&mut bytes, "core-1.0/gcc/", b'5', 0o755, 0, "");
        append_header(&mut bytes, "core-1.0/gcc/common.c", b'0', 0o644, 4, "");
        append_data(&mut bytes, b"core");
        append_header(&mut bytes, "core-1.0/README", b'0', 0o644, 4, "");
        append_data(&mut bytes, b"core");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&core, bytes).unwrap();
        let gpp = tmp.join("gpp.tar");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "gpp-1.0/", b'5', 0o755, 0, "");
        append_header(&mut bytes, "gpp-1.0/gcc/", b'5', 0o755, 0, "");
        append_header(&mut bytes, "gpp-1.0/gcc/cp.c", b'0', 0o644, 3, "");
        append_data(&mut bytes, b"gpp");
        append_header(&mut bytes, "gpp-1.0/README", b'0', 0o644, 3, "");
        append_data(&mut bytes, b"gpp");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&gpp, bytes).unwrap();

        unpack_archive(&core, &dest, false).unwrap();
        unpack_archive(&gpp, &dest, false).unwrap();

        // The overlay MERGED into the existing tree (the g++ add-on tarball
        // over its gcc core tree): the core file survives beside the
        // overlay's, and the colliding file is replaced by the overlay.
        assert_eq!(fs::read(dest.join("gcc/common.c")).unwrap(), b"core");
        assert_eq!(fs::read(dest.join("gcc/cp.c")).unwrap(), b"gpp");
        assert_eq!(fs::read(dest.join("README")).unwrap(), b"gpp");
    }

    #[test]
    fn strip_top_unpack_refuses_a_symlink_top() {
        let tmp = temp_dir("td-tar-symlink-top-test");
        let outside = tmp.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), b"k").unwrap();
        let tar = tmp.join("test.tar");
        let dest = tmp.join("src");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "top", b'2', 0o777, 0, outside.to_str().unwrap());
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        // A sole top-level SYMLINK (even to a real directory) must refuse:
        // following it would hoist files out of the link's target tree.
        let err = unpack_archive(&tar, &dest, false).expect_err("symlink top must red");

        assert!(
            err.contains("exactly one top-level directory"),
            "got: {err}"
        );
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn strip_top_unpack_refuses_file_directory_kind_clash() {
        let tmp = temp_dir("td-tar-overlay-clash-test");
        let dest = tmp.join("src");
        fs::create_dir_all(dest.join("gcc")).unwrap();
        let tar = tmp.join("clash.tar");
        let mut bytes = Vec::new();
        append_header(&mut bytes, "x-1.0/", b'5', 0o755, 0, "");
        append_header(&mut bytes, "x-1.0/gcc", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"x");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = unpack_archive(&tar, &dest, false).expect_err("kind clash must red");

        assert!(err.contains("disagree on file vs directory"), "got: {err}");
    }

    #[test]
    fn extracts_pax_global_and_extended_headers() {
        // The exact git-archive shape (glibc 2.16.0's source tarball is one):
        // a leading `pax_global_header` carrying `comment=<sha>`, then ustar
        // entries. Plus per-entry pax 'x' headers overriding a long `path` and a
        // long symlink `linkpath` beyond the 100-byte ustar name fields.
        let tmp = temp_dir("td-tar-pax-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(
            &mut bytes,
            b'g',
            &pax_record("comment", "0123456789abcdef0123456789abcdef01234567"),
        );
        append_header(&mut bytes, "README", b'0', 0o644, 5, "");
        append_data(&mut bytes, b"hello");
        let long_path = format!("glibc-2.16.0/{}/locale.c", "sysdeps/unix".repeat(10));
        append_pax(&mut bytes, b'x', &pax_record("path", &long_path));
        append_header(&mut bytes, "ustar-fallback", b'0', 0o644, 3, "");
        append_data(&mut bytes, b"loc");
        let long_target = format!("{}/libc.so", "a".repeat(120));
        append_pax(&mut bytes, b'x', &pax_record("linkpath", &long_target));
        append_header(&mut bytes, "libc-link", b'2', 0o777, 0, "placeholder");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        // The global header was skipped and the plain entry landed under its name.
        assert_eq!(fs::read(out.join("README")).unwrap(), b"hello");
        // The pax `path` override won over the ustar "ustar-fallback" name.
        assert_eq!(fs::read(out.join(&long_path)).unwrap(), b"loc");
        assert!(!out.join("ustar-fallback").exists());
        // The pax `linkpath` override won over the ustar "placeholder" target.
        assert_eq!(
            fs::read_link(out.join("libc-link")).unwrap(),
            PathBuf::from(&long_target)
        );
    }

    #[test]
    fn refuses_oversized_pax_header_before_allocating() {
        let tmp = temp_dir("td-tar-oversized-pax-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_header(
            &mut bytes,
            "pax_global_header",
            b'g',
            0o644,
            MAX_GNU_LONG_FIELD_BYTES + 1,
            "",
        );
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("oversized pax header should fail");

        assert!(err.contains("metadata-header field"), "got: {err}");
    }

    #[test]
    fn refuses_malformed_pax_record_length() {
        let tmp = temp_dir("td-tar-bad-pax-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        // A record whose declared length runs past the header body.
        let bad = b"999 path=x\n".to_vec();
        append_pax(&mut bytes, b'x', &bad);
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"y");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("bad pax record should fail");

        assert!(err.contains("pax record length"), "got: {err}");
    }

    #[test]
    fn refuses_overflowing_pax_record_length() {
        // A declared length near `usize::MAX` at a non-zero offset: the old
        // `pos + rec_len` bound wrapped (a debug-build overflow panic). The
        // overflow-safe `rec_len > records.len() - pos` reds it cleanly instead.
        let tmp = temp_dir("td-tar-overflow-pax-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut records = pax_record("path", "ok");
        records.extend_from_slice(format!("{} x=y\n", usize::MAX).as_bytes());
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', &records);
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"y");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("overflowing pax length should fail");

        assert!(err.contains("pax record length"), "got: {err}");
    }

    #[test]
    fn refuses_pax_size_override() {
        // A `size` record would override the following entry's byte count;
        // ignoring it desyncs the block stream, so we reject rather than skip.
        let tmp = temp_dir("td-tar-pax-size-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', &pax_record("size", "999999"));
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"y");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("pax size override should fail");

        assert!(err.contains("`size` override is unsupported"), "got: {err}");
    }

    #[test]
    fn refuses_pax_global_naming_override() {
        // A `path`/`linkpath` in a GLOBAL header is an archive-wide default we
        // cannot faithfully apply per entry; surface it rather than mis-extract.
        let tmp = temp_dir("td-tar-pax-global-name-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'g', &pax_record("path", "archive-wide"));
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"y");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("global naming override should fail");

        assert!(err.contains("global `path`/`linkpath`"), "got: {err}");
    }

    #[test]
    fn refuses_pax_record_missing_equals() {
        let tmp = temp_dir("td-tar-pax-noeq-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', b"7 nope\n");
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"y");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("record without '=' should fail");

        assert!(err.contains("missing '='"), "got: {err}");
    }

    #[test]
    fn pax_empty_value_unsets_a_pending_name() {
        // A `path` record followed by an empty `path=` (the pax "unset"
        // directive) leaves the ustar name in force.
        let tmp = temp_dir("td-tar-pax-unset-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut records = pax_record("path", "override-name");
        records.extend_from_slice(&pax_record("path", ""));
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', &records);
        append_header(&mut bytes, "real-ustar-name", b'0', 0o644, 3, "");
        append_data(&mut bytes, b"abc");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        assert_eq!(fs::read(out.join("real-ustar-name")).unwrap(), b"abc");
        assert!(!out.join("override-name").exists());
    }

    #[test]
    fn extracts_pax_multi_record_extended_header() {
        // One 'x' header carrying several records: an ignored `mtime`, then a
        // `path` and a `linkpath` that both apply to the following symlink.
        let tmp = temp_dir("td-tar-pax-multi-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut records = pax_record("mtime", "1700000000.0");
        records.extend_from_slice(&pax_record("path", "long/sym/name"));
        records.extend_from_slice(&pax_record("linkpath", "long/target/path"));
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', &records);
        append_header(&mut bytes, "ustar-sym", b'2', 0o777, 0, "placeholder");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        assert_eq!(
            fs::read_link(out.join("long/sym/name")).unwrap(),
            PathBuf::from("long/target/path")
        );
    }

    #[test]
    fn refuses_pax_path_traversal() {
        // A pax `path` override runs through the same safe_join guard as a
        // ustar name, so `../escape` is refused.
        let tmp = temp_dir("td-tar-pax-traversal-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', &pax_record("path", "../escape"));
        append_header(&mut bytes, "innocent", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"z");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("pax path traversal should fail");

        assert!(err.contains("escaping tar path"), "got: {err}");
    }

    #[test]
    fn refuses_dangling_pax_extended_header_at_eof() {
        // An 'x' header carrying only an ignored record (mtime) with no entry
        // after it: a truncated archive must red, not silently succeed. The
        // pending name slots stay empty here, so this is only caught by the
        // separate pending-extended tracking.
        let tmp = temp_dir("td-tar-pax-dangling-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', &pax_record("mtime", "1700000000.0"));
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("dangling pax extended header should fail");

        assert!(
            err.contains("pax extended header without a following entry"),
            "got: {err}"
        );
    }

    #[test]
    fn accepts_empty_global_naming_unset() {
        // A global `path=` with an empty value is the pax "unset" directive: a
        // no-op that installs no archive-wide override, so it must NOT red (a
        // non-empty global path/linkpath still does).
        let tmp = temp_dir("td-tar-pax-global-unset-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'g', &pax_record("path", ""));
        append_header(&mut bytes, "file", b'0', 0o644, 2, "");
        append_data(&mut bytes, b"hi");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        extract_tar(&tar, &out).unwrap();

        assert_eq!(fs::read(out.join("file")).unwrap(), b"hi");
    }

    #[test]
    fn refuses_pax_non_numeric_length() {
        let tmp = temp_dir("td-tar-pax-nan-len-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', b"XX path=y\n");
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"z");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("non-numeric pax length should fail");

        assert!(err.contains("bad length"), "got: {err}");
    }

    #[test]
    fn refuses_pax_record_missing_length_separator() {
        // A record body with no space: the length/keyword separator is missing.
        let tmp = temp_dir("td-tar-pax-nospace-test");
        let tar = tmp.join("test.tar");
        let out = tmp.join("out");
        let mut bytes = Vec::new();
        append_pax(&mut bytes, b'x', b"12path=value\n");
        append_header(&mut bytes, "file", b'0', 0o644, 1, "");
        append_data(&mut bytes, b"z");
        bytes.extend_from_slice(&[0u8; BLOCK * 2]);
        fs::write(&tar, bytes).unwrap();

        let err = extract_tar(&tar, &out).expect_err("missing length separator should fail");

        assert!(err.contains("missing length separator"), "got: {err}");
    }

    fn temp_dir(prefix: &str) -> crate::test_temp::TempDir {
        crate::test_temp::TempDir::new(prefix).unwrap()
    }

    fn append_long_name(out: &mut Vec<u8>, path: &str) {
        append_header(out, "././@LongLink", b'L', 0o644, long_field_len(path), "");
        append_long_value(out, path);
    }

    fn append_long_link(out: &mut Vec<u8>, target: &str) {
        append_header(
            out,
            "././@LongLink",
            b'K',
            0o644,
            long_field_len(target),
            "",
        );
        append_long_value(out, target);
    }

    fn append_long_value(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(value.as_bytes());
        out.push(0);
        pad(out);
    }

    // A pax header block (typeflag 'g' or 'x') followed by its record-stream body.
    fn append_pax(out: &mut Vec<u8>, typeflag: u8, records: &[u8]) {
        let size = u64::try_from(records.len()).unwrap();
        append_header(out, "pax_header", typeflag, 0o644, size, "");
        out.extend_from_slice(records);
        pad(out);
    }

    // Build one `"<len> <key>=<value>\n"` pax record whose self-referential
    // length field counts its own digits.
    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let fixed = key.len() + value.len() + 3; // ' ' + '=' + '\n'
        let mut len = fixed + 1;
        while len.to_string().len() + fixed != len {
            len = len.to_string().len() + fixed;
        }
        format!("{len} {key}={value}\n").into_bytes()
    }

    fn long_field_len(value: &str) -> u64 {
        u64::try_from(value.len() + 1).unwrap()
    }

    fn append_header(
        out: &mut Vec<u8>,
        name: &str,
        typeflag: u8,
        mode: u32,
        size: u64,
        link: &str,
    ) {
        append_header_mtime(out, name, typeflag, mode, size, link, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn append_header_mtime(
        out: &mut Vec<u8>,
        name: &str,
        typeflag: u8,
        mode: u32,
        size: u64,
        link: &str,
        mtime: u64,
    ) {
        let mut header = [0u8; BLOCK];
        write_bytes(&mut header, 0, 100, name.as_bytes());
        write_octal(&mut header, 100, 8, u64::from(mode));
        write_octal(&mut header, 108, 8, 0);
        write_octal(&mut header, 116, 8, 0);
        write_octal(&mut header, 124, 12, size);
        write_octal(&mut header, 136, 12, mtime);
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        header[156] = typeflag;
        write_bytes(&mut header, 157, 100, link.as_bytes());
        write_bytes(&mut header, 257, 6, b"ustar");
        write_bytes(&mut header, 263, 2, b"00");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        write_checksum(&mut header, sum);
        out.extend_from_slice(&header);
    }

    fn append_data(out: &mut Vec<u8>, data: &[u8]) {
        out.extend_from_slice(data);
        pad(out);
    }

    fn pad(out: &mut Vec<u8>) {
        let rem = out.len() % BLOCK;
        if rem != 0 {
            out.resize(out.len() + (BLOCK - rem), 0);
        }
    }

    fn write_bytes(header: &mut [u8; BLOCK], start: usize, len: usize, value: &[u8]) {
        assert!(value.len() <= len);
        header[start..start + value.len()].copy_from_slice(value);
    }

    fn write_octal(header: &mut [u8; BLOCK], start: usize, len: usize, value: u64) {
        let text = format!("{value:0width$o}\0", width = len - 1);
        write_bytes(header, start, len, text.as_bytes());
    }

    fn write_checksum(header: &mut [u8; BLOCK], sum: u32) {
        let text = format!("{sum:06o}\0 ");
        write_bytes(header, 148, 8, text.as_bytes());
    }
}
