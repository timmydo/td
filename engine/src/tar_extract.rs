use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

const BLOCK: usize = 512;
const MAX_TAR_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GNU_LONG_FIELD_BYTES: u64 = 1024 * 1024;
const MAX_TAR_ENTRIES: u64 = 1_000_000;
const MAX_TAR_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024 * 1024;

pub fn extract_tar_reader<R: Read>(
    file: &mut R,
    source_name: &str,
    dest: &Path,
) -> Result<(), String> {
    extract_tar_reader_with_options(file, source_name, dest, MAX_TAR_ENTRY_BYTES, None)
}

/// `cli_umask` enables interactive extraction: restrictive initial files,
/// archived directory metadata, and final modes filtered by the caller's mask.
/// None retains source preparation's historical metadata policy.
pub fn extract_tar_reader_with_options<R: Read>(
    file: &mut R,
    source_name: &str,
    dest: &Path,
    max_entry_bytes: u64,
    cli_umask: Option<u32>,
) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    let mut pending_path: Option<PathBuf> = None;
    let mut pending_link: Option<PathBuf> = None;
    // A pax extended ('x') header MUST be followed by an ordinary entry it
    // describes. Unlike GNU 'L'/'K' this is tracked separately, because an 'x'
    // header can carry only records we ignore (e.g. `mtime`) or an unset that
    // leaves both name slots empty -- so a dangling 'x' at end-of-archive would
    // otherwise pass unnoticed. A global ('g') header is archive-wide and does
    // NOT require a following entry, so it never sets this.
    let mut pending_extended = false;
    let mut budget = ArchiveBudget::default();
    let mut directories = BTreeMap::new();
    loop {
        let mut header = [0u8; BLOCK];
        read_tar_block(file, &mut header, source_name)?;
        if header.iter().all(|b| *b == 0) {
            ensure_no_dangling_header(&pending_path, &pending_link, pending_extended)?;
            let mut second = [0u8; BLOCK];
            read_tar_block(file, &mut second, source_name)?;
            if second.iter().any(|byte| *byte != 0) {
                return Err(format!(
                    "tar archive {source_name} has only one zero end marker"
                ));
            }
            validate_zero_padding(file, source_name)?;
            break;
        }
        validate_header_checksum(&header)
            .map_err(|e| format!("validate tar header from {source_name}: {e}"))?;

        let mut entry = Entry::parse(&header)?;
        validate_entry_size(&entry, max_entry_bytes)?;
        // Charge the header before reading its body or creating its path. In
        // particular, metadata records count too: hostile GNU/PAX headers
        // must not bypass the aggregate archive budget.
        budget.account(&entry)?;
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
                let records = read_entry_bytes(file, entry.size)
                    .map_err(|e| format!("read pax global header {}: {e}", entry.path.display()))?;
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
                pending_extended = true;
                skip_padding(file, entry.size)
                    .map_err(|e| format!("skip padding after {}: {e}", entry.path.display()))?;
                continue;
            }
            _ => {}
        }
        // An ordinary entry: it consumes any pax extended header that preceded
        // it (only 'L'/'K'/'g'/'x' arms `continue` above, so reaching here means
        // a real file/dir/link is being extracted).
        pending_extended = false;
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
                if let Some(mask) = cli_umask {
                    let mode = entry.mode & !mask & 0o777;
                    // Keep owner access until descendants and metadata finish.
                    fs::set_permissions(&out, fs::Permissions::from_mode(mode | 0o700))
                        .map_err(|e| format!("chmod {}: {e}", out.display()))?;
                    directories.insert(out.clone(), (mode, entry.mtime));
                }
                skip_entry_data(file, entry.size)
                    .map_err(|e| format!("skip {}: {e}", entry.path.display()))?;
            }
            EntryKind::Regular => {
                ensure_parent_dirs(dest, &entry.path)?;
                remove_existing_non_dir(&out)?;
                let mut options = File::options();
                options.write(true).create(true).truncate(true);
                if cli_umask.is_some() {
                    options.mode(0o600);
                }
                let mut out_file = options
                    .open(&out)
                    .map_err(|e| format!("create {}: {e}", out.display()))?;
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
                    .set_permissions(fs::Permissions::from_mode(match cli_umask {
                        Some(mask) => entry.mode & !mask & 0o777,
                        None => entry.mode,
                    }))
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
    // Descendants first: read-only parent modes must not block their metadata.
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, (mode, mtime)) in directories {
        let dir = File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        if let Some(mtime) = mtime {
            set_file_mtime(&dir, mtime)?;
        }
        dir.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

fn read_tar_block<R: Read>(
    file: &mut R,
    block: &mut [u8; BLOCK],
    source_name: &str,
) -> Result<(), String> {
    let mut filled = 0usize;
    while filled < BLOCK {
        let remaining = block
            .get_mut(filled..)
            .ok_or_else(|| "internal tar header offset exceeded its block".to_string())?;
        match file.read(remaining) {
            Ok(0) => {
                return Err(format!(
                    "truncated tar header from {source_name}: read {filled} of {BLOCK} bytes"
                ));
            }
            Ok(count) => {
                filled = filled
                    .checked_add(count)
                    .ok_or_else(|| "tar header byte count overflowed usize".to_string())?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read tar header from {source_name}: {e}")),
        }
    }
    Ok(())
}

fn validate_zero_padding<R: Read>(file: &mut R, source_name: &str) -> Result<(), String> {
    let mut padding = [0u8; 8192];
    loop {
        match file.read(&mut padding) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                let bytes = padding
                    .get(..count)
                    .ok_or_else(|| "tar padding read exceeded its fixed buffer".to_string())?;
                if bytes.iter().any(|byte| *byte != 0) {
                    return Err(format!(
                        "tar archive {source_name} has nonzero data after its end markers"
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read trailing tar padding from {source_name}: {e}")),
        }
    }
}

#[derive(Default)]
struct ArchiveBudget {
    entries: u64,
    payload: u64,
}

impl ArchiveBudget {
    fn account(&mut self, entry: &Entry) -> Result<(), String> {
        let entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| "tar entry count exceeds u64".to_string())?;
        if entries > MAX_TAR_ENTRIES {
            return Err(format!(
                "tar archive has more than {MAX_TAR_ENTRIES} entries"
            ));
        }
        let payload = self
            .payload
            .checked_add(entry.size)
            .ok_or_else(|| "tar payload length exceeds u64".to_string())?;
        if payload > MAX_TAR_PAYLOAD_BYTES {
            return Err(format!(
                "tar archive payload exceeds {MAX_TAR_PAYLOAD_BYTES} byte limit"
            ));
        }
        // Commit only after every check succeeds so an error cannot leave a
        // reusable budget object in a misleading partially-accounted state.
        self.entries = entries;
        self.payload = payload;
        Ok(())
    }
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

fn validate_entry_size(entry: &Entry, max_entry_bytes: u64) -> Result<(), String> {
    match &entry.kind {
        EntryKind::Regular => {
            if entry.size > max_entry_bytes {
                return Err(format!(
                    "tar entry {} is too large: {} bytes exceeds {} byte limit",
                    entry.path.display(),
                    entry.size,
                    max_entry_bytes
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
        return Err(format!("refusing escaping tar path {}", rel.display()));
    }
    Ok(root.join(rel))
}

fn has_parent_component(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

fn ensure_no_dangling_header(
    pending_path: &Option<PathBuf>,
    pending_link: &Option<PathBuf>,
    pending_extended: bool,
) -> Result<(), String> {
    if pending_path.is_some() {
        return Err("tar ended after GNU long name without a following entry".to_string());
    }
    if pending_link.is_some() {
        return Err("tar ended after GNU long link without a following entry".to_string());
    }
    if pending_extended {
        return Err("tar ended after a pax extended header without a following entry".to_string());
    }
    Ok(())
}

fn ensure_parent_dirs(root: &Path, rel: &Path) -> Result<(), String> {
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    ensure_no_symlink_ancestors(root, parent)
}

fn ensure_no_symlink_ancestors(root: &Path, rel: &Path) -> Result<(), String> {
    if rel.is_absolute() || has_parent_component(rel) {
        return Err(format!("refusing escaping tar path {}", rel.display()));
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
            _ => return Err(format!("refusing escaping tar path {}", rel.display())),
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
        let len_str = std::str::from_utf8(len_bytes)
            .map_err(|_| "pax record length is not ascii".to_string())?;
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
                    "pax `size` override is unsupported (would desync the entry stream)"
                        .to_string(),
                );
            }
            b"path" if !global => set_or_clear(pending_path, value)?,
            b"linkpath" if !global => set_or_clear(pending_link, value)?,
            // A global `path`/`linkpath` with an empty value is the pax "unset"
            // directive: it installs no archive-wide default, so it is a no-op
            // here (we retain no global naming state) rather than an error.
            b"path" | b"linkpath" if value.is_empty() => {}
            // A non-empty global `path`/`linkpath` is an archive-wide default we
            // cannot faithfully apply per entry, so surface it.
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
