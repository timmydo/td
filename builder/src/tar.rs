use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    let tmp = match dest.file_name() {
        Some(name) => {
            let mut t = name.to_os_string();
            t.push(".unpack-tmp");
            dest.with_file_name(t)
        }
        None => {
            return Err(format!(
                "unpack {}: destination {} has no file name to place a tmp dir beside",
                tarball.display(),
                dest.display()
            ))
        }
    };
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| format!("clear {}: {e}", tmp.display()))?;
    }
    extract_into(&tmp)?;
    let mut tops = fs::read_dir(&tmp)
        .map_err(|e| format!("read {}: {e}", tmp.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read {}: {e}", tmp.display()))?;
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
    fs::remove_dir_all(&tmp).map_err(|e| format!("clear {}: {e}", tmp.display()))?;
    Ok(())
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
            EntryKind::PaxGlobal => {
                // Archive-wide pax defaults — e.g. the `comment=<sha>` global
                // header git-archive prepends to every tarball it makes. We
                // apply none of them (path/linkpath are per-entry concerns and
                // size/mode/mtime come from each entry's own ustar header), but
                // we still parse the stream so a global `size` or `path`/
                // `linkpath` default we cannot faithfully honour reds the unpack
                // rather than being silently ignored.
                let records = read_entry_bytes(file, entry.size).map_err(|e| {
                    format!("read pax global header {}: {e}", entry.path.display())
                })?;
                apply_pax(&records, true, &mut pending_path, &mut pending_link)?;
                skip_padding(file, entry.size)
                    .map_err(|e| format!("skip padding after {}: {e}", entry.path.display()))?;
                continue;
            }
            EntryKind::PaxExtended => {
                // Per-entry pax overrides for the entry that follows. We honour
                // `path`/`linkpath` (long or non-ASCII names — the pax analogue
                // of GNU 'L'/'K'); other records (mtime/uid/...) are left to the
                // following entry's ustar header, and a `size` override is
                // rejected because ignoring it would desync the block stream.
                let records = read_entry_bytes(file, entry.size).map_err(|e| {
                    format!("read pax extended header {}: {e}", entry.path.display())
                })?;
                apply_pax(&records, false, &mut pending_path, &mut pending_link)?;
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
                // Restore the archived mtime (like GNU tar/Guix), BEFORE the
                // chmod below may drop owner-write — futimens on the still-open
                // fd is unaffected by the file's mode. Preserving mtimes keeps a
                // tarball's shipped generated files (e.g. gcc's pre-built bison
                // parser c-parse.c) NEWER than their sources, so `make` treats
                // them as up-to-date and never invokes the maintainer-mode
                // regenerators (bison/flex/gperf) absent from the seed. Without
                // this every extracted file got a "now" mtime in EXTRACTION
                // order, inverting that relationship whenever the generated file
                // is stored before its source and forcing a spurious rebuild
                // (re #469).
                if let Some(mtime) = entry.mtime {
                    set_file_mtime(&out_file, mtime)
                        .map_err(|e| format!("set mtime on {}: {e}", out.display()))?;
                }
                // chmod through the open fd (fchmod), not a second path lookup —
                // we already hold the descriptor, and it can't be swapped for a
                // symlink between create and chmod the way `&out` could.
                out_file
                    .set_permissions(fs::Permissions::from_mode(entry.mode))
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
            EntryKind::LongName
            | EntryKind::LongLink
            | EntryKind::PaxGlobal
            | EntryKind::PaxExtended => {
                return Err(format!(
                    "internal tar metadata-header state error at {}",
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
    mtime: Option<u64>,
    kind: EntryKind,
}

enum EntryKind {
    Directory,
    Regular,
    Hardlink { target: PathBuf },
    Symlink { target: PathBuf },
    LongName,
    LongLink,
    // POSIX pax extended headers. `PaxExtended` (typeflag 'x') carries records
    // that override the NEXT entry's fields; `PaxGlobal` (typeflag 'g') carries
    // archive-wide defaults. Both have a record-stream body of `size` bytes.
    PaxGlobal,
    PaxExtended,
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
        // mtime: octal seconds since the epoch. GNU base-256 encodes far-future
        // or negative times as high-bit bytes that are not octal; parse_octal
        // rejects those, and `.ok()` falls back to `None` (leave the file's
        // extraction-time mtime) rather than fail an otherwise-valid archive —
        // the same octal-only limitation `size`/`mode` above already carry.
        let mtime = parse_octal(field(header, 136, 12)?).ok();
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
            b'g' => EntryKind::PaxGlobal,
            b'x' => EntryKind::PaxExtended,
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
            mtime,
            kind,
        })
    }
}

/// Stamp an extracted regular file's mtime from its tar header, via the still-open
/// fd (unaffected by the file's mode). An out-of-range value — a pathological
/// far-future header, or any post-2038 time on a 32-bit `time_t` platform where
/// `SystemTime` tops out — is SKIPPED (the file keeps its extraction-time mtime)
/// rather than failing the whole unpack, mirroring the base-256 `None` fallback in
/// `Entry::parse`. A genuine `set_times` syscall failure still propagates, so a
/// real regression in mtime preservation reds loudly rather than silently
/// reverting to "now".
fn set_file_mtime(file: &File, secs: u64) -> Result<(), String> {
    let Some(when) = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs)) else {
        return Ok(());
    };
    file.set_times(fs::FileTimes::new().set_modified(when))
        .map_err(|e| e.to_string())
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
        EntryKind::LongName
        | EntryKind::LongLink
        | EntryKind::PaxGlobal
        | EntryKind::PaxExtended => {
            if entry.size > MAX_GNU_LONG_FIELD_BYTES {
                return Err(format!(
                    "tar metadata-header field {} is too large: {} bytes exceeds {} byte limit",
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

/// Read a metadata-header body (a pax record stream) into memory verbatim —
/// unlike `read_entry_string` it neither strips trailing NULs nor requires the
/// whole body be UTF-8, since a pax stream is length-delimited records.
/// `validate_entry_size` has already bounded `bytes` to `MAX_GNU_LONG_FIELD_BYTES`.
fn read_entry_bytes<R: Read>(reader: &mut R, bytes: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tar pax header too large"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// Apply the records of a pax header. Records are `"<len> <key>=<value>\n"`,
/// where `<len>` is the whole record's byte length including its own digits and
/// the trailing newline. For an extended header (`global == false`) we honour
/// `path`/`linkpath` into the pending long-name/long-link slots the next entry
/// consumes — the pax analogue of GNU 'L'/'K'. Records we do not model (mtime,
/// uid, gid, uname, gname, comment, ...) are skipped; the following entry's
/// ustar header supplies those fields.
///
/// Two records are rejected rather than ignored, because ignoring them would
/// silently mis-extract: a `size` record overrides the following entry's byte
/// count, so skipping it would desync the block stream for any file too large
/// for the 12-byte ustar size field; and a `path`/`linkpath` in a *global*
/// header (`global == true`) is an archive-wide default we cannot faithfully
/// apply per entry. An empty value is the pax "unset" directive: it clears any
/// pending name so the ustar header stays in force. A malformed length or a
/// record that overruns the buffer reds the unpack rather than mis-parsing.
fn apply_pax(
    records: &[u8],
    global: bool,
    pending_path: &mut Option<PathBuf>,
    pending_link: &mut Option<PathBuf>,
) -> Result<(), String> {
    let mut pos = 0usize;
    while let Some(rest) = records.get(pos..) {
        if rest.is_empty() {
            break;
        }
        let space = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| "pax record missing length separator".to_string())?;
        let len_bytes = rest.get(..space).unwrap_or(&[]);
        let len_str =
            std::str::from_utf8(len_bytes).map_err(|_| "pax record length is not ascii".to_string())?;
        let rec_len = len_str
            .parse::<usize>()
            .map_err(|_| format!("pax record has a bad length `{len_str}`"))?;
        // `rec_len` must cover at least "<len> =\n" and stay within the buffer.
        // Written as `rec_len > records.len() - pos` (never `pos + rec_len`,
        // which can wrap in debug builds when `rec_len` nears `usize::MAX`);
        // `records.len() - pos` cannot underflow because `rest` is non-empty.
        if rec_len <= space + 1 || rec_len > records.len() - pos {
            return Err(format!(
                "pax record length {rec_len} overruns the {}-byte header",
                records.len()
            ));
        }
        let record = records.get(pos..pos + rec_len).unwrap_or(&[]);
        if record.last() != Some(&b'\n') {
            return Err("pax record is not newline-terminated".to_string());
        }
        // Payload `<key>=<value>` sits between the length's space and the newline.
        let kv = record.get(space + 1..rec_len - 1).unwrap_or(&[]);
        let eq = kv
            .iter()
            .position(|b| *b == b'=')
            .ok_or_else(|| "pax record is missing '='".to_string())?;
        let key = kv.get(..eq).unwrap_or(&[]);
        let value = kv.get(eq + 1..).unwrap_or(&[]);
        match key {
            b"size" => {
                return Err(
                    "pax `size` override is unsupported (would desync the entry stream)".to_string(),
                );
            }
            b"path" if !global => set_or_clear(pending_path, value)?,
            b"linkpath" if !global => set_or_clear(pending_link, value)?,
            b"path" | b"linkpath" => {
                return Err("pax global `path`/`linkpath` override is unsupported".to_string());
            }
            _ => {}
        }
        pos += rec_len;
    }
    Ok(())
}

/// Set a pending long-name slot from a pax value, or clear it when the value is
/// empty (the pax "unset" directive).
fn set_or_clear(slot: &mut Option<PathBuf>, value: &[u8]) -> Result<(), String> {
    let s = pax_value_string(value)?;
    *slot = if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    };
    Ok(())
}

fn pax_value_string(value: &[u8]) -> Result<String, String> {
    String::from_utf8(value.to_vec()).map_err(|e| format!("pax value is not utf-8: {e}"))
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

        let generated = fs::metadata(out.join("c-parse.c")).unwrap().modified().unwrap();
        let source = fs::metadata(out.join("c-parse.y")).unwrap().modified().unwrap();
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

        assert!(err.contains("exactly one top-level directory"), "got: {err}");
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

        assert!(err.contains("unsafe tar path"), "got: {err}");
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
