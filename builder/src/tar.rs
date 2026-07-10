use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;
const MAX_TAR_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GNU_LONG_FIELD_BYTES: u64 = 1024 * 1024;

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

pub fn extract_tar_xz(tar_xz: &Path, dest: &Path) -> Result<(), String> {
    let data = fs::read(tar_xz).map_err(|e| format!("read {}: {e}", tar_xz.display()))?;
    let bytes = crate::xz::decompress(&data)?;
    let mut reader = Cursor::new(bytes);
    extract_tar_reader(&mut reader, &tar_xz.display().to_string(), dest)
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
        let n = f.read(&mut magic).map_err(|e| format!("read {}: {e}", tarball.display()))?;
        match magic.get(..n).unwrap_or(&[]) {
            m if m.starts_with(&[0x1f, 0x8b]) => extract_tar_gz(tarball, d),
            m if m.starts_with(b"BZh") => extract_tar_bz2(tarball, d),
            m if m.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) => {
                extract_tar_xz(tarball, d)
            }
            _ => extract_tar(tarball, d),
        }
    };
    if keep_top {
        return extract_into(dest);
    }
    // Strip the top-level dir: extract beside dest (same filesystem, so the
    // child renames below are atomic moves), then hoist the unique top dir's
    // children into dest.
    let tmp = dest.with_extension("unpack-tmp");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| format!("clear {}: {e}", tmp.display()))?;
    }
    extract_into(&tmp)?;
    let mut tops = fs::read_dir(&tmp)
        .map_err(|e| format!("read {}: {e}", tmp.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read {}: {e}", tmp.display()))?;
    let top = match (tops.pop(), tops.is_empty()) {
        (Some(t), true) if t.path().is_dir() => t.path(),
        _ => {
            return Err(format!(
                "unpack {}: stripping the top level needs exactly one top-level directory",
                tarball.display()
            ))
        }
    };
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    for ent in fs::read_dir(&top).map_err(|e| format!("read {}: {e}", top.display()))? {
        let ent = ent.map_err(|e| format!("read {}: {e}", top.display()))?;
        let to = dest.join(ent.file_name());
        fs::rename(ent.path(), &to).map_err(|e| {
            format!("move {} -> {}: {e}", ent.path().display(), to.display())
        })?;
    }
    fs::remove_dir_all(&tmp).map_err(|e| format!("clear {}: {e}", tmp.display()))?;
    Ok(())
}

pub fn extract_tar_reader<R: Read>(
    file: &mut R,
    source_name: &str,
    dest: &Path,
) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    let mut pending_path: Option<PathBuf> = None;
    let mut pending_link: Option<PathBuf> = None;
    loop {
        let mut header = [0u8; BLOCK];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                ensure_no_pending_long_name(&pending_path, &pending_link)?;
                break;
            }
            Err(e) => return Err(format!("read tar header from {source_name}: {e}")),
        }
        if header.iter().all(|b| *b == 0) {
            ensure_no_pending_long_name(&pending_path, &pending_link)?;
            break;
        }
        validate_header_checksum(&header)
            .map_err(|e| format!("validate tar header from {source_name}: {e}"))?;

        let mut entry = Entry::parse(&header)?;
        validate_entry_size(&entry)?;
        match &entry.kind {
            EntryKind::LongName => {
                pending_path =
                    Some(read_entry_string(file, entry.size).map_err(|e| {
                        format!("read GNU long name {}: {e}", entry.path.display())
                    })?);
                skip_padding(file, entry.size)
                    .map_err(|e| format!("skip padding after {}: {e}", entry.path.display()))?;
                continue;
            }
            EntryKind::LongLink => {
                pending_link =
                    Some(read_entry_string(file, entry.size).map_err(|e| {
                        format!("read GNU long link {}: {e}", entry.path.display())
                    })?);
                skip_padding(file, entry.size)
                    .map_err(|e| format!("skip padding after {}: {e}", entry.path.display()))?;
                continue;
            }
            _ => {}
        }
        if let Some(path) = pending_path.take() {
            if path.as_os_str().is_empty() {
                return Err("GNU long name entry had an empty path".to_string());
            }
            entry.path = path;
        }
        if let Some(target) = pending_link.take() {
            if target.as_os_str().is_empty() {
                return Err("GNU long link entry had an empty target".to_string());
            }
            match &mut entry.kind {
                EntryKind::Hardlink { target: link } | EntryKind::Symlink { target: link } => {
                    *link = target;
                }
                _ => {
                    return Err(format!(
                        "GNU long link entry applied to non-link {}",
                        entry.path.display()
                    ));
                }
            }
        }
        let out = safe_join(dest, &entry.path)?;
        match entry.kind {
            EntryKind::Directory => {
                ensure_directory(dest, &entry.path, &out)?;
                skip_entry_data(file, entry.size)
                    .map_err(|e| format!("skip {}: {e}", entry.path.display()))?;
            }
            EntryKind::Regular => {
                ensure_parent_dirs(dest, &entry.path)?;
                remove_existing_non_dir(&out)?;
                let mut out_file =
                    File::create(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
                copy_exact(file, &mut out_file, entry.size)
                    .map_err(|e| format!("extract {}: {e}", entry.path.display()))?;
                fs::set_permissions(&out, fs::Permissions::from_mode(entry.mode))
                    .map_err(|e| format!("chmod {}: {e}", out.display()))?;
            }
            EntryKind::Symlink { target } => {
                ensure_parent_dirs(dest, &entry.path)?;
                remove_existing_non_dir(&out)?;
                symlink(&target, &out).map_err(|e| {
                    format!("symlink {} -> {}: {e}", out.display(), target.display())
                })?;
                skip_entry_data(file, entry.size)
                    .map_err(|e| format!("skip {}: {e}", entry.path.display()))?;
            }
            EntryKind::Hardlink { target } => {
                ensure_parent_dirs(dest, &entry.path)?;
                let target_out = safe_join(dest, &target)?;
                let target_parent = target.parent().unwrap_or_else(|| Path::new(""));
                ensure_no_symlink_ancestors(dest, target_parent)?;
                let meta = fs::symlink_metadata(&target_out)
                    .map_err(|e| format!("hardlink target {}: {e}", target.display()))?;
                if meta.file_type().is_symlink() {
                    return Err(format!("refusing hardlink to symlink {}", target.display()));
                }
                remove_existing_non_dir(&out)?;
                fs::hard_link(&target_out, &out).map_err(|e| {
                    format!(
                        "hardlink {} -> {}: {e}",
                        out.display(),
                        target_out.display()
                    )
                })?;
                skip_entry_data(file, entry.size)
                    .map_err(|e| format!("skip {}: {e}", entry.path.display()))?;
            }
            EntryKind::LongName | EntryKind::LongLink => {
                return Err(format!(
                    "internal tar long-name state error at {}",
                    entry.path.display()
                ));
            }
        }
        skip_padding(file, entry.size)
            .map_err(|e| format!("skip padding after {}: {e}", entry.path.display()))?;
    }
    Ok(())
}

struct Entry {
    path: PathBuf,
    size: u64,
    mode: u32,
    kind: EntryKind,
}

enum EntryKind {
    Directory,
    Regular,
    Hardlink { target: PathBuf },
    Symlink { target: PathBuf },
    LongName,
    LongLink,
}

impl Entry {
    fn parse(header: &[u8; BLOCK]) -> Result<Entry, String> {
        let name = tar_string(field(header, 0, 100)?)?;
        let prefix = tar_string(field(header, 345, 155)?)?;
        let path = if prefix.is_empty() {
            PathBuf::from(name)
        } else {
            PathBuf::from(prefix).join(name)
        };
        if path.as_os_str().is_empty() {
            return Err("tar entry with empty path".into());
        }
        let size = parse_octal(field(header, 124, 12)?)?;
        let mode = u32::try_from(parse_octal(field(header, 100, 8)?)? & 0o7777)
            .map_err(|_| "tar mode did not fit u32".to_string())?;
        let typeflag = *header
            .get(156)
            .ok_or_else(|| "tar header missing typeflag".to_string())?;
        let kind = match typeflag {
            0 | b'0' => EntryKind::Regular,
            b'1' => EntryKind::Hardlink {
                target: PathBuf::from(tar_string(field(header, 157, 100)?)?),
            },
            b'5' => EntryKind::Directory,
            b'2' => EntryKind::Symlink {
                target: PathBuf::from(tar_string(field(header, 157, 100)?)?),
            },
            b'L' => EntryKind::LongName,
            b'K' => EntryKind::LongLink,
            other => {
                return Err(format!(
                    "unsupported tar entry type {} for {}",
                    other,
                    path.display()
                ));
            }
        };
        Ok(Entry {
            path,
            size,
            mode,
            kind,
        })
    }
}

fn field(header: &[u8; BLOCK], start: usize, len: usize) -> Result<&[u8], String> {
    header
        .get(start..start + len)
        .ok_or_else(|| "tar header field out of bounds".to_string())
}

fn tar_string(bytes: &[u8]) -> Result<String, String> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let raw = bytes
        .get(..end)
        .ok_or_else(|| "tar string field out of bounds".to_string())?;
    String::from_utf8(raw.to_vec()).map_err(|e| format!("tar path is not utf-8: {e}"))
}

fn parse_octal(bytes: &[u8]) -> Result<u64, String> {
    let text = tar_string(bytes)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(trimmed, 8).map_err(|e| format!("bad tar octal field `{trimmed}`: {e}"))
}

fn validate_header_checksum(header: &[u8; BLOCK]) -> Result<(), String> {
    let stored = parse_octal(field(header, 148, 8)?)?;
    let computed: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                u64::from(b' ')
            } else {
                u64::from(*b)
            }
        })
        .sum();
    if stored != computed {
        return Err(format!(
            "tar header checksum mismatch: stored {stored:o}, computed {computed:o}"
        ));
    }
    Ok(())
}

fn validate_entry_size(entry: &Entry) -> Result<(), String> {
    match &entry.kind {
        EntryKind::Regular => {
            if entry.size > MAX_TAR_ENTRY_BYTES {
                return Err(format!(
                    "tar entry {} is too large: {} bytes exceeds {} byte limit",
                    entry.path.display(),
                    entry.size,
                    MAX_TAR_ENTRY_BYTES
                ));
            }
        }
        EntryKind::LongName | EntryKind::LongLink => {
            if entry.size > MAX_GNU_LONG_FIELD_BYTES {
                return Err(format!(
                    "GNU tar long-name field {} is too large: {} bytes exceeds {} byte limit",
                    entry.path.display(),
                    entry.size,
                    MAX_GNU_LONG_FIELD_BYTES
                ));
            }
        }
        EntryKind::Directory | EntryKind::Hardlink { .. } | EntryKind::Symlink { .. } => {
            if entry.size != 0 {
                return Err(format!(
                    "tar entry {} of this type must not carry {} data bytes",
                    entry.path.display(),
                    entry.size
                ));
            }
        }
    }
    Ok(())
}

fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    if rel.is_absolute() || has_parent_component(rel) {
        return Err(format!("refusing unsafe tar path {}", rel.display()));
    }
    Ok(root.join(rel))
}

fn has_parent_component(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

fn ensure_no_pending_long_name(
    pending_path: &Option<PathBuf>,
    pending_link: &Option<PathBuf>,
) -> Result<(), String> {
    if pending_path.is_some() {
        return Err("tar ended after GNU long name without a following entry".to_string());
    }
    if pending_link.is_some() {
        return Err("tar ended after GNU long link without a following entry".to_string());
    }
    Ok(())
}

fn ensure_parent_dirs(root: &Path, rel: &Path) -> Result<(), String> {
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    ensure_no_symlink_ancestors(root, parent)
}

fn ensure_no_symlink_ancestors(root: &Path, rel: &Path) -> Result<(), String> {
    if rel.is_absolute() || has_parent_component(rel) {
        return Err(format!("refusing unsafe tar path {}", rel.display()));
    }
    let mut cur = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(name) => {
                cur.push(name);
                match fs::symlink_metadata(&cur) {
                    Ok(meta) => {
                        let ft = meta.file_type();
                        if ft.is_symlink() {
                            return Err(format!(
                                "refusing tar path through symlink {}",
                                cur.display()
                            ));
                        }
                        if !ft.is_dir() {
                            return Err(format!(
                                "refusing tar path through non-directory {}",
                                cur.display()
                            ));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        fs::create_dir(&cur)
                            .map_err(|err| format!("mkdir {}: {err}", cur.display()))?;
                    }
                    Err(e) => return Err(format!("stat {}: {e}", cur.display())),
                }
            }
            Component::CurDir => {}
            _ => return Err(format!("refusing unsafe tar path {}", rel.display())),
        }
    }
    Ok(())
}

fn ensure_directory(root: &Path, rel: &Path, out: &Path) -> Result<(), String> {
    ensure_parent_dirs(root, rel)?;
    match fs::symlink_metadata(out) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(format!("refusing directory over symlink {}", out.display()));
            }
            if !ft.is_dir() {
                return Err(format!("refusing directory over file {}", out.display()));
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(out).map_err(|err| format!("mkdir {}: {err}", out.display()))
        }
        Err(e) => Err(format!("stat {}: {e}", out.display())),
    }
}

fn remove_existing_non_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_dir() {
                return Err(format!("refusing to replace directory {}", path.display()));
            }
            fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

fn copy_exact<R: Read, W: Write>(reader: &mut R, writer: &mut W, bytes: u64) -> io::Result<()> {
    let mut limited = reader.take(bytes);
    let copied = io::copy(&mut limited, writer)?;
    if copied != bytes {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("short tar entry: copied {copied} of {bytes} bytes"),
        ));
    }
    Ok(())
}

fn skip_entry_data<R: Read>(reader: &mut R, bytes: u64) -> io::Result<()> {
    let mut limited = reader.take(bytes);
    let copied = io::copy(&mut limited, &mut io::sink())?;
    if copied != bytes {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("short tar entry: skipped {copied} of {bytes} bytes"),
        ));
    }
    Ok(())
}

fn read_entry_string<R: Read>(reader: &mut R, bytes: u64) -> io::Result<PathBuf> {
    let len = usize::try_from(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tar string too large"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    while matches!(buf.last(), Some(0)) {
        let _ = buf.pop();
    }
    String::from_utf8(buf).map(PathBuf::from).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tar string is not utf-8: {e}"),
        )
    })
}

fn skip_padding<R: Read>(reader: &mut R, size: u64) -> io::Result<()> {
    let rem = size % 512;
    if rem == 0 {
        return Ok(());
    }
    let pad = 512 - rem;
    skip_entry_data(reader, pad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn refuses_parent_components() {
        let err = safe_join(Path::new("/tmp/root"), Path::new("../x")).expect_err("unsafe path");
        assert!(err.contains("unsafe tar path"));
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

        assert!(err.contains("long-name field"), "got: {err}");
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

    fn temp_dir(prefix: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
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
        let mut header = [0u8; BLOCK];
        write_bytes(&mut header, 0, 100, name.as_bytes());
        write_octal(&mut header, 100, 8, u64::from(mode));
        write_octal(&mut header, 108, 8, 0);
        write_octal(&mut header, 116, 8, 0);
        write_octal(&mut header, 124, 12, size);
        write_octal(&mut header, 136, 12, 0);
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
