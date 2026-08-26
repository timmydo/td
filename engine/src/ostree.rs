//! The bounded OSTree metadata subset used by td's foreign-payload importer.
//!
//! This is deliberately not a general GVariant implementation.  An imported
//! deploy needs exactly commit, dirtree, and dirmeta objects; accepting more
//! types would enlarge the parser without enlarging the package format td can
//! publish.  OSTree metadata values use big-endian byte order, while GVariant
//! framing offsets are always little-endian.

use crate::{gzip, sha256};
use std::collections::BTreeSet;

pub const CHECKSUM_BYTES: usize = 32;
pub const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_TREE_ENTRIES: usize = 262_144;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_COMMIT_METADATA_ENTRIES: usize = 64;
pub const MAX_COMMIT_ARRAY_ENTRIES: usize = 4_096;
pub const MAX_ARCHIVE_INPUT_BYTES: usize = 257 * 1024 * 1024;
pub const MAX_ARCHIVE_FILE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_ARCHIVE_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_SYMLINK_TARGET_BYTES: usize = 4 * 1024 - 1;

const DIRECTORY_TYPE: u32 = 0o040000;
const REGULAR_TYPE: u32 = 0o100000;
const SYMLINK_TYPE: u32 = 0o120000;
const FILE_TYPE_MASK: u32 = 0o170000;
const SPECIAL_PERMISSION_BITS: u32 = 0o7000;
const PERMISSION_BITS: u32 = 0o7777;
const ALLOWED_MODE_BITS: u32 = FILE_TYPE_MASK | PERMISSION_BITS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Checksum([u8; CHECKSUM_BYTES]);

impl Checksum {
    pub fn from_hex(value: &str) -> Result<Checksum, String> {
        if value.len() != CHECKSUM_BYTES * 2 {
            return Err(
                "OSTree checksum must be exactly 64 lowercase hexadecimal characters".into(),
            );
        }
        let mut bytes = [0u8; CHECKSUM_BYTES];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let at = index
                .checked_mul(2)
                .ok_or_else(|| "OSTree checksum offset overflow".to_string())?;
            let end = at
                .checked_add(2)
                .ok_or_else(|| "OSTree checksum offset overflow".to_string())?;
            let pair = value
                .get(at..end)
                .ok_or_else(|| "OSTree checksum is truncated".to_string())?;
            if !pair
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            {
                return Err("OSTree checksum must use lowercase hexadecimal".into());
            }
            *slot = u8::from_str_radix(pair, 16)
                .map_err(|_| "OSTree checksum is not hexadecimal".to_string())?;
        }
        Ok(Checksum(bytes))
    }

    fn from_bytes(value: &[u8], what: &str) -> Result<Checksum, String> {
        let bytes: [u8; CHECKSUM_BYTES] = value
            .try_into()
            .map_err(|_| format!("{what} is not a 32-byte OSTree checksum"))?;
        Ok(Checksum(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; CHECKSUM_BYTES] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(CHECKSUM_BYTES * 2);
        for byte in self.0 {
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
        out
    }
}

fn hex_digit(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + (nibble - 10)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub root_tree: Checksum,
    pub root_meta: Checksum,
    pub metadata: CommitMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitMetadata {
    pub collection_binding: Option<String>,
    pub collection_refs_binding: Vec<(String, String)>,
    pub ref_binding: Vec<String>,
    pub xa_ref: Option<String>,
    pub from_commit: Option<String>,
    pub download_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub subsets: Vec<String>,
    pub flatpak_metadata: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub checksum: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub tree: Checksum,
    pub meta: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirtree {
    pub files: Vec<FileEntry>,
    pub directories: Vec<DirectoryEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dirmeta {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ArchiveFile {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub kind: ArchiveFileKind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArchiveFileKind {
    Regular(Vec<u8>),
    Symlink(String),
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveHeader {
    size: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    target: String,
}

pub fn verify_object(expected: Checksum, bytes: &[u8], what: &str) -> Result<(), String> {
    let mut hasher = sha256::Sha256::new();
    hasher.update(bytes);
    let actual = hasher.finalize();
    if actual != expected.0 {
        return Err(format!(
            "{what} checksum mismatch: expected {}, got {}",
            expected.to_hex(),
            sha256::to_base16(&actual)
        ));
    }
    Ok(())
}

/// Authenticate and parse `(a{sv}aya(say)sstayay)`, OSTree's commit object.
pub fn parse_commit_verified(expected: Checksum, bytes: &[u8]) -> Result<Commit, String> {
    require_metadata_size(bytes, "commit")?;
    verify_object(expected, bytes, "OSTree commit")?;
    parse_commit(bytes)
}

fn parse_commit(bytes: &[u8]) -> Result<Commit, String> {
    require_metadata_size(bytes, "commit")?;
    let (ends, data_end) = tuple_variable_ends(bytes, 6, "commit")?;
    let metadata_end = item(&ends, 0, "commit metadata offset")?;
    let parent_end = item(&ends, 1, "commit parent offset")?;
    let related_end = item(&ends, 2, "commit related-object offset")?;
    let subject_end = item(&ends, 3, "commit subject offset")?;
    let body_end = item(&ends, 4, "commit body offset")?;
    let root_tree_end = item(&ends, 5, "commit root-tree offset")?;

    let metadata = parse_commit_metadata(slice(bytes, 0, metadata_end, "commit metadata")?)?;
    let parent = slice(bytes, metadata_end, parent_end, "commit parent")?;
    if !(parent.is_empty() || parent.len() == CHECKSUM_BYTES) {
        return Err("commit parent must be empty or one checksum".into());
    }
    let related = slice(bytes, parent_end, related_end, "commit related objects")?;
    if !related.is_empty() {
        return Err("commit related objects are outside td's bounded deploy subset".into());
    }
    parse_string(
        slice(bytes, related_end, subject_end, "commit subject")?,
        "commit subject",
    )?;
    parse_string(
        slice(bytes, subject_end, body_end, "commit body")?,
        "commit body",
    )?;

    let timestamp_start = align_up(body_end, 8, "commit timestamp")?;
    require_zero_padding(bytes, body_end, timestamp_start, "commit timestamp")?;
    let root_tree_start = timestamp_start
        .checked_add(8)
        .ok_or_else(|| "commit timestamp offset overflow".to_string())?;
    let root_tree = Checksum::from_bytes(
        slice(bytes, root_tree_start, root_tree_end, "commit root tree")?,
        "commit root tree",
    )?;
    let root_meta = Checksum::from_bytes(
        slice(bytes, root_tree_end, data_end, "commit root metadata")?,
        "commit root metadata",
    )?;
    Ok(Commit {
        root_tree,
        root_meta,
        metadata,
    })
}

fn parse_commit_metadata(bytes: &[u8]) -> Result<CommitMetadata, String> {
    let entries = variable_array_aligned(bytes, MAX_COMMIT_METADATA_ENTRIES, 8, "commit metadata")?;
    let mut metadata = CommitMetadata::default();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let (key, value, value_type) = parse_metadata_entry(entry)?;
        if !seen.insert(key) {
            return Err(format!("commit metadata key {key:?} is duplicated"));
        }
        match key {
            "ostree.collection-binding" => {
                require_variant_type(value_type, "s", key)?;
                metadata.collection_binding = Some(parse_string(value, key)?.to_string());
            }
            "ostree.collection-refs-binding" => {
                require_variant_type(value_type, "a(ss)", key)?;
                metadata.collection_refs_binding = parse_string_pairs(value, key)?;
            }
            "ostree.ref-binding" => {
                require_variant_type(value_type, "as", key)?;
                metadata.ref_binding = parse_string_array(value, key)?;
            }
            "xa.ref" => {
                require_variant_type(value_type, "s", key)?;
                metadata.xa_ref = Some(parse_string(value, key)?.to_string());
            }
            "xa.from_commit" => {
                require_variant_type(value_type, "s", key)?;
                metadata.from_commit = Some(parse_string(value, key)?.to_string());
            }
            "xa.download-size" => {
                require_variant_type(value_type, "t", key)?;
                metadata.download_size = Some(u64_be_exact(value, key)?);
            }
            "xa.installed-size" => {
                require_variant_type(value_type, "t", key)?;
                metadata.installed_size = Some(u64_be_exact(value, key)?);
            }
            "xa.subsets" => {
                require_variant_type(value_type, "as", key)?;
                metadata.subsets = parse_string_array(value, key)?;
            }
            "xa.metadata" => {
                require_variant_type(value_type, "s", key)?;
                metadata.flatpak_metadata = Some(parse_string(value, key)?.to_string());
            }
            "xa.extra-data-sources" => {
                return Err("commit uses refused xa.extra-data-sources payloads".into());
            }
            _ => {
                return Err(format!(
                    "commit metadata key {key:?} is outside td's reviewed Flathub subset"
                ));
            }
        }
    }
    Ok(metadata)
}

fn parse_metadata_entry(bytes: &[u8]) -> Result<(&str, &[u8], &str), String> {
    let (ends, data_end) = tuple_variable_ends(bytes, 1, "commit metadata entry")?;
    let key_end = item(&ends, 0, "commit metadata key offset")?;
    let key = parse_string(
        slice(bytes, 0, key_end, "commit metadata key")?,
        "commit metadata key",
    )?;
    let value_start = align_up(key_end, 8, "commit metadata value")?;
    require_zero_padding(
        bytes,
        key_end,
        value_start,
        "commit metadata value alignment",
    )?;
    let variant = slice(bytes, value_start, data_end, "commit metadata variant")?;
    let separator = variant
        .iter()
        .rposition(|byte| *byte == 0)
        .ok_or_else(|| format!("commit metadata key {key:?} has no variant type separator"))?;
    let value = slice(variant, 0, separator, "commit metadata variant value")?;
    let type_start = separator
        .checked_add(1)
        .ok_or_else(|| "commit metadata variant type offset overflow".to_string())?;
    let value_type = std::str::from_utf8(slice(
        variant,
        type_start,
        variant.len(),
        "commit metadata variant type",
    )?)
    .map_err(|_| format!("commit metadata key {key:?} has a non-UTF-8 variant type"))?;
    if value_type.is_empty()
        || !value_type
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'@')
    {
        return Err(format!(
            "commit metadata key {key:?} has an invalid variant type"
        ));
    }
    Ok((key, value, value_type))
}

fn require_variant_type(actual: &str, expected: &str, key: &str) -> Result<(), String> {
    if actual != expected {
        return Err(format!(
            "commit metadata key {key:?} has type {actual:?}, expected {expected:?}"
        ));
    }
    Ok(())
}

fn parse_string_array(bytes: &[u8], what: &str) -> Result<Vec<String>, String> {
    let values = variable_array(bytes, MAX_COMMIT_ARRAY_ENTRIES, what)?;
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        strings.push(parse_string(value, what)?.to_string());
    }
    Ok(strings)
}

fn parse_string_pairs(bytes: &[u8], what: &str) -> Result<Vec<(String, String)>, String> {
    let values = variable_array(bytes, MAX_COMMIT_ARRAY_ENTRIES, what)?;
    let mut pairs = Vec::with_capacity(values.len());
    for value in values {
        let (ends, data_end) = tuple_variable_ends(value, 1, what)?;
        let first_end = item(&ends, 0, "commit metadata string-pair offset")?;
        let first = parse_string(slice(value, 0, first_end, what)?, what)?.to_string();
        let second = parse_string(slice(value, first_end, data_end, what)?, what)?.to_string();
        pairs.push((first, second));
    }
    Ok(pairs)
}

/// Authenticate and parse `(a(say)a(sayay))`, an OSTree directory tree.
pub fn parse_dirtree_verified(expected: Checksum, bytes: &[u8]) -> Result<Dirtree, String> {
    require_metadata_size(bytes, "dirtree")?;
    verify_object(expected, bytes, "OSTree dirtree")?;
    parse_dirtree(bytes)
}

fn parse_dirtree(bytes: &[u8]) -> Result<Dirtree, String> {
    require_metadata_size(bytes, "dirtree")?;
    let (ends, data_end) = tuple_variable_ends(bytes, 1, "dirtree")?;
    let files_end = item(&ends, 0, "dirtree file-array offset")?;
    let file_values = variable_array(
        slice(bytes, 0, files_end, "dirtree files")?,
        MAX_TREE_ENTRIES,
        "dirtree files",
    )?;
    let remaining = MAX_TREE_ENTRIES
        .checked_sub(file_values.len())
        .ok_or_else(|| "dirtree entry count overflow".to_string())?;
    let directory_values = variable_array(
        slice(bytes, files_end, data_end, "dirtree directories")?,
        remaining,
        "dirtree directories",
    )?;

    let mut files = Vec::with_capacity(file_values.len());
    for value in file_values {
        files.push(parse_file_entry(value)?);
    }
    let mut directories = Vec::with_capacity(directory_values.len());
    for value in directory_values {
        directories.push(parse_directory_entry(value)?);
    }
    validate_sorted_names(&files, &directories)?;
    Ok(Dirtree { files, directories })
}

/// Authenticate and parse `(uuua(ayay))`, refusing imported xattrs.
pub fn parse_dirmeta_verified(expected: Checksum, bytes: &[u8]) -> Result<Dirmeta, String> {
    require_metadata_size(bytes, "dirmeta")?;
    verify_object(expected, bytes, "OSTree dirmeta")?;
    parse_dirmeta(bytes)
}

fn parse_dirmeta(bytes: &[u8]) -> Result<Dirmeta, String> {
    require_metadata_size(bytes, "dirmeta")?;
    if bytes.len() != 12 {
        return Err("dirmeta carries xattrs or has a noncanonical fixed header".into());
    }
    let uid = u32_be(bytes, 0, "dirmeta uid")?;
    let gid = u32_be(bytes, 4, "dirmeta gid")?;
    let mode = u32_be(bytes, 8, "dirmeta mode")?;
    if mode & !ALLOWED_MODE_BITS != 0 {
        return Err(format!("dirmeta mode {mode:#o} carries undefined bits"));
    }
    if mode & FILE_TYPE_MASK != DIRECTORY_TYPE {
        return Err(format!("dirmeta mode {mode:#o} is not a directory"));
    }
    if mode & SPECIAL_PERMISSION_BITS != 0 {
        return Err(format!(
            "dirmeta mode {mode:#o} carries setid or sticky permission bits"
        ));
    }
    Ok(Dirmeta { uid, gid, mode })
}

/// Decode and authenticate one archive-z2 `.filez` regular file or symlink.
///
/// Archive bytes are transport encoding, so their SHA-256 is not the object
/// identity. The identity covers OSTree's canonical uncompressed content
/// header followed by regular-file contents. Symlinks have no content body.
pub fn decode_archive_file_verified(
    expected: Checksum,
    bytes: &[u8],
) -> Result<ArchiveFile, String> {
    require_archive_input_size(bytes.len())?;
    let header_len = usize::try_from(u32_be(bytes, 0, "archive header length")?)
        .map_err(|_| "archive header length does not fit usize".to_string())?;
    if header_len == 0 || header_len > MAX_ARCHIVE_HEADER_BYTES {
        return Err(format!(
            "archive header size must be in 1..={MAX_ARCHIVE_HEADER_BYTES} bytes"
        ));
    }
    require_zero_padding(bytes, 4, 8, "archive header alignment")?;
    let header_end = 8usize
        .checked_add(header_len)
        .ok_or_else(|| "archive header end overflow".to_string())?;
    let header = parse_archive_header(slice(bytes, 8, header_end, "archive header")?)?;
    let body = slice(bytes, header_end, bytes.len(), "archive body")?;

    if header.mode & !ALLOWED_MODE_BITS != 0 {
        return Err(format!(
            "archive file mode {:#o} carries undefined bits",
            header.mode
        ));
    }
    if header.mode & SPECIAL_PERMISSION_BITS != 0 {
        return Err(format!(
            "archive file mode {:#o} carries setid or sticky permission bits",
            header.mode
        ));
    }

    let kind = match header.mode & FILE_TYPE_MASK {
        REGULAR_TYPE => {
            if !header.target.is_empty() {
                return Err("regular archive file carries a symlink target".into());
            }
            let size = usize::try_from(header.size)
                .map_err(|_| "archive file size does not fit usize".to_string())?;
            let contents = gzip::decompress_raw_exact(body, size, MAX_ARCHIVE_FILE_BYTES)
                .map_err(|error| format!("archive file payload: {error}"))?;
            verify_archive_content(expected, &header, &contents)?;
            ArchiveFileKind::Regular(contents)
        }
        SYMLINK_TYPE => {
            if header.size != 0 {
                return Err("archive symlink declares a nonzero content size".into());
            }
            if !body.is_empty() {
                return Err("archive symlink carries a content body".into());
            }
            if header.target.is_empty() || header.target.len() > MAX_SYMLINK_TARGET_BYTES {
                return Err(format!(
                    "archive symlink target length must be in 1..={MAX_SYMLINK_TARGET_BYTES} bytes"
                ));
            }
            verify_archive_content(expected, &header, &[])?;
            ArchiveFileKind::Symlink(header.target.clone())
        }
        kind => {
            return Err(format!(
                "archive file mode {:#o} has refused file type {kind:#o}",
                header.mode
            ));
        }
    };

    Ok(ArchiveFile {
        uid: header.uid,
        gid: header.gid,
        mode: header.mode,
        kind,
    })
}

fn require_archive_input_size(size: usize) -> Result<(), String> {
    if size == 0 || size > MAX_ARCHIVE_INPUT_BYTES {
        return Err(format!(
            "archive file size must be in 1..={MAX_ARCHIVE_INPUT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn parse_archive_header(bytes: &[u8]) -> Result<ArchiveHeader, String> {
    let (ends, data_end) = tuple_variable_ends(bytes, 1, "archive header")?;
    let target_end = item(&ends, 0, "archive symlink-target offset")?;
    if target_end < 24 {
        return Err("archive header overlaps its fixed fields".into());
    }
    if target_end != data_end {
        return Err("archive file carries refused xattrs".into());
    }
    let size = u64_be_at(bytes, 0, "archive file size")?;
    let uid = u32_be(bytes, 8, "archive file uid")?;
    let gid = u32_be(bytes, 12, "archive file gid")?;
    let mode = u32_be(bytes, 16, "archive file mode")?;
    let rdev = u32_be(bytes, 20, "archive file device number")?;
    if rdev != 0 {
        return Err("archive file carries a nonzero device number".into());
    }
    let target = parse_string(
        slice(bytes, 24, target_end, "archive symlink target")?,
        "archive symlink target",
    )?
    .to_string();
    Ok(ArchiveHeader {
        size,
        uid,
        gid,
        mode,
        target,
    })
}

fn verify_archive_content(
    expected: Checksum,
    header: &ArchiveHeader,
    contents: &[u8],
) -> Result<(), String> {
    let canonical_header = canonical_content_header(header)?;
    let mut hasher = sha256::Sha256::new();
    hasher.update(&canonical_header);
    hasher.update(contents);
    let actual = hasher.finalize();
    if actual != expected.0 {
        return Err(format!(
            "OSTree file checksum mismatch: expected {}, got {}",
            expected.to_hex(),
            sha256::to_base16(&actual)
        ));
    }
    Ok(())
}

fn canonical_content_header(header: &ArchiveHeader) -> Result<Vec<u8>, String> {
    let target_len = header
        .target
        .len()
        .checked_add(1)
        .ok_or_else(|| "archive symlink target length overflow".to_string())?;
    let data_len = 16usize
        .checked_add(target_len)
        .ok_or_else(|| "OSTree content header length overflow".to_string())?;
    let width = minimal_serialized_width(data_len, 1, "OSTree content header")?;
    let variant_len = data_len
        .checked_add(width)
        .ok_or_else(|| "OSTree content header length overflow".to_string())?;
    let variant_len_u32 = u32::try_from(variant_len)
        .map_err(|_| "OSTree content header length does not fit u32".to_string())?;
    let total_len = 8usize
        .checked_add(variant_len)
        .ok_or_else(|| "OSTree content header length overflow".to_string())?;
    let mut bytes = Vec::with_capacity(total_len);
    bytes.extend_from_slice(&variant_len_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&header.uid.to_be_bytes());
    bytes.extend_from_slice(&header.gid.to_be_bytes());
    bytes.extend_from_slice(&header.mode.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(header.target.as_bytes());
    bytes.push(0);
    push_offset(&mut bytes, data_len, width, "OSTree content header")?;
    Ok(bytes)
}

fn parse_file_entry(bytes: &[u8]) -> Result<FileEntry, String> {
    let (ends, data_end) = tuple_variable_ends(bytes, 1, "dirtree file entry")?;
    let name_end = item(&ends, 0, "dirtree file name offset")?;
    let name = parse_name(slice(bytes, 0, name_end, "dirtree file name")?)?;
    let checksum = Checksum::from_bytes(
        slice(bytes, name_end, data_end, "dirtree file checksum")?,
        "dirtree file checksum",
    )?;
    Ok(FileEntry { name, checksum })
}

fn parse_directory_entry(bytes: &[u8]) -> Result<DirectoryEntry, String> {
    let (ends, data_end) = tuple_variable_ends(bytes, 2, "dirtree directory entry")?;
    let name_end = item(&ends, 0, "dirtree directory name offset")?;
    let tree_end = item(&ends, 1, "dirtree directory tree offset")?;
    let name = parse_name(slice(bytes, 0, name_end, "dirtree directory name")?)?;
    let tree = Checksum::from_bytes(
        slice(bytes, name_end, tree_end, "dirtree directory tree checksum")?,
        "dirtree directory tree checksum",
    )?;
    let meta = Checksum::from_bytes(
        slice(
            bytes,
            tree_end,
            data_end,
            "dirtree directory metadata checksum",
        )?,
        "dirtree directory metadata checksum",
    )?;
    Ok(DirectoryEntry { name, tree, meta })
}

fn validate_sorted_names(
    files: &[FileEntry],
    directories: &[DirectoryEntry],
) -> Result<(), String> {
    for pair in files.windows(2) {
        let Some(left) = pair.first() else {
            return Err("dirtree file ordering window is empty".into());
        };
        let Some(right) = pair.get(1) else {
            return Err("dirtree file ordering window is incomplete".into());
        };
        if left.name >= right.name {
            return Err("dirtree file names are not strictly sorted".into());
        }
    }
    for pair in directories.windows(2) {
        let Some(left) = pair.first() else {
            return Err("dirtree directory ordering window is empty".into());
        };
        let Some(right) = pair.get(1) else {
            return Err("dirtree directory ordering window is incomplete".into());
        };
        if left.name >= right.name {
            return Err("dirtree directory names are not strictly sorted".into());
        }
    }
    for file in files {
        if directories
            .binary_search_by(|directory| directory.name.as_str().cmp(file.name.as_str()))
            .is_ok()
        {
            return Err(format!(
                "dirtree name {:?} is both a file and a directory",
                file.name
            ));
        }
    }
    Ok(())
}

fn parse_name(bytes: &[u8]) -> Result<String, String> {
    let name = parse_string(bytes, "dirtree name")?;
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "dirtree name length must be in 1..={MAX_NAME_BYTES} bytes"
        ));
    }
    if name == "." || name == ".." || name.as_bytes().contains(&b'/') {
        return Err(format!("unsafe dirtree name {name:?}"));
    }
    Ok(name.to_string())
}

fn parse_string<'a>(bytes: &'a [u8], what: &str) -> Result<&'a str, String> {
    let Some((&last, text)) = bytes.split_last() else {
        return Err(format!("{what} is not NUL terminated"));
    };
    if last != 0 || text.contains(&0) {
        return Err(format!("{what} is not one canonical NUL-terminated string"));
    }
    std::str::from_utf8(text).map_err(|_| format!("{what} is not UTF-8"))
}

fn variable_array<'a>(bytes: &'a [u8], limit: usize, what: &str) -> Result<Vec<&'a [u8]>, String> {
    variable_array_aligned(bytes, limit, 1, what)
}

fn variable_array_aligned<'a>(
    bytes: &'a [u8],
    limit: usize,
    alignment: usize,
    what: &str,
) -> Result<Vec<&'a [u8]>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let width = framing_width(bytes.len());
    let last_start = bytes
        .len()
        .checked_sub(width)
        .ok_or_else(|| format!("{what} has no framing offset"))?;
    let data_end = read_offset(bytes, last_start, width, what)?;
    if data_end > last_start {
        return Err(format!(
            "{what} final framing offset crosses its offset table"
        ));
    }
    let table_bytes = bytes
        .len()
        .checked_sub(data_end)
        .ok_or_else(|| format!("{what} framing length underflow"))?;
    if table_bytes % width != 0 {
        return Err(format!("{what} framing table is not width-aligned"));
    }
    let count = table_bytes / width;
    if count > limit {
        return Err(format!("{what} has {count} entries; limit is {limit}"));
    }
    require_minimal_framing_width(data_end, count, width, what)?;
    let mut values = Vec::with_capacity(count);
    let mut previous_end = 0usize;
    for index in 0..count {
        let start = if index == 0 {
            0
        } else {
            align_up(previous_end, alignment, what)?
        };
        require_zero_padding(bytes, previous_end, start, what)?;
        let offset_at = data_end
            .checked_add(
                index
                    .checked_mul(width)
                    .ok_or_else(|| format!("{what} offset index overflow"))?,
            )
            .ok_or_else(|| format!("{what} offset position overflow"))?;
        let end = read_offset(bytes, offset_at, width, what)?;
        if end <= start {
            return Err(format!("{what} has a non-increasing framing offset"));
        }
        if end > data_end {
            return Err(format!("{what} has a framing offset outside its data"));
        }
        values.push(slice(bytes, start, end, what)?);
        previous_end = end;
    }
    if previous_end != data_end {
        return Err(format!("{what} framing offsets do not consume the data"));
    }
    Ok(values)
}

fn tuple_variable_ends(
    bytes: &[u8],
    count: usize,
    what: &str,
) -> Result<(Vec<usize>, usize), String> {
    let width = framing_width(bytes.len());
    let table_bytes = count
        .checked_mul(width)
        .ok_or_else(|| format!("{what} framing size overflow"))?;
    let data_end = bytes
        .len()
        .checked_sub(table_bytes)
        .ok_or_else(|| format!("{what} is shorter than its framing table"))?;
    require_minimal_framing_width(data_end, count, width, what)?;
    let mut stored = Vec::with_capacity(count);
    for index in 0..count {
        let at = data_end
            .checked_add(
                index
                    .checked_mul(width)
                    .ok_or_else(|| format!("{what} framing index overflow"))?,
            )
            .ok_or_else(|| format!("{what} framing position overflow"))?;
        stored.push(read_offset(bytes, at, width, what)?);
    }
    stored.reverse();
    let mut previous = 0usize;
    for end in &stored {
        if *end < previous || *end > data_end {
            return Err(format!("{what} has invalid variable-field framing"));
        }
        previous = *end;
    }
    Ok((stored, data_end))
}

fn framing_width(container_len: usize) -> usize {
    if container_len <= u8::MAX as usize {
        1
    } else if container_len <= u16::MAX as usize {
        2
    } else {
        4
    }
}

fn require_minimal_framing_width(
    data_len: usize,
    offset_count: usize,
    actual: usize,
    what: &str,
) -> Result<(), String> {
    let expected = minimal_serialized_width(data_len, offset_count, what)?;
    if actual != expected {
        return Err(format!("{what} uses a non-minimal framing width"));
    }
    Ok(())
}

fn minimal_serialized_width(
    data_len: usize,
    offset_count: usize,
    what: &str,
) -> Result<usize, String> {
    for width in [1usize, 2, 4] {
        let table_len = offset_count
            .checked_mul(width)
            .ok_or_else(|| format!("{what} framing size overflow"))?;
        let total = data_len
            .checked_add(table_len)
            .ok_or_else(|| format!("{what} framing size overflow"))?;
        if framing_width(total) == width {
            return Ok(width);
        }
    }
    Err(format!("{what} has no supported framing width"))
}

fn push_offset(output: &mut Vec<u8>, value: usize, width: usize, what: &str) -> Result<(), String> {
    match width {
        1 => {
            output.push(u8::try_from(value).map_err(|_| format!("{what} offset does not fit u8"))?)
        }
        2 => output.extend_from_slice(
            &u16::try_from(value)
                .map_err(|_| format!("{what} offset does not fit u16"))?
                .to_le_bytes(),
        ),
        4 => output.extend_from_slice(
            &u32::try_from(value)
                .map_err(|_| format!("{what} offset does not fit u32"))?
                .to_le_bytes(),
        ),
        _ => return Err(format!("{what} uses an unsupported framing width")),
    }
    Ok(())
}

fn read_offset(bytes: &[u8], at: usize, width: usize, what: &str) -> Result<usize, String> {
    let raw = slice(
        bytes,
        at,
        at.checked_add(width)
            .ok_or_else(|| format!("{what} offset width overflow"))?,
        what,
    )?;
    let value = match width {
        1 => usize::from(
            *raw.first()
                .ok_or_else(|| format!("{what} offset is empty"))?,
        ),
        2 => usize::from(u16::from_le_bytes(
            raw.try_into()
                .map_err(|_| format!("{what} has a malformed 16-bit offset"))?,
        )),
        4 => usize::try_from(u32::from_le_bytes(
            raw.try_into()
                .map_err(|_| format!("{what} has a malformed 32-bit offset"))?,
        ))
        .map_err(|_| format!("{what} offset does not fit usize"))?,
        _ => return Err(format!("{what} uses an unsupported framing width")),
    };
    Ok(value)
}

fn require_metadata_size(bytes: &[u8], what: &str) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_METADATA_BYTES {
        return Err(format!(
            "{what} size must be in 1..={MAX_METADATA_BYTES} bytes"
        ));
    }
    Ok(())
}

fn align_up(value: usize, alignment: usize, what: &str) -> Result<usize, String> {
    let mask = alignment
        .checked_sub(1)
        .ok_or_else(|| format!("{what} has zero alignment"))?;
    value
        .checked_add(mask)
        .map(|sum| sum & !mask)
        .ok_or_else(|| format!("{what} alignment overflow"))
}

fn require_zero_padding(bytes: &[u8], start: usize, end: usize, what: &str) -> Result<(), String> {
    if slice(bytes, start, end, what)?
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(format!("{what} has nonzero alignment padding"));
    }
    Ok(())
}

fn u32_be(bytes: &[u8], at: usize, what: &str) -> Result<u32, String> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| format!("{what} offset overflow"))?;
    Ok(u32::from_be_bytes(
        slice(bytes, at, end, what)?
            .try_into()
            .map_err(|_| format!("{what} is truncated"))?,
    ))
}

fn u64_be_exact(bytes: &[u8], what: &str) -> Result<u64, String> {
    let raw: [u8; 8] = bytes
        .try_into()
        .map_err(|_| format!("{what} is not one 64-bit value"))?;
    Ok(u64::from_be_bytes(raw))
}

fn u64_be_at(bytes: &[u8], at: usize, what: &str) -> Result<u64, String> {
    let end = at
        .checked_add(8)
        .ok_or_else(|| format!("{what} offset overflow"))?;
    Ok(u64::from_be_bytes(
        slice(bytes, at, end, what)?
            .try_into()
            .map_err(|_| format!("{what} is truncated"))?,
    ))
}

fn item(values: &[usize], index: usize, what: &str) -> Result<usize, String> {
    values
        .get(index)
        .copied()
        .ok_or_else(|| format!("missing {what}"))
}

fn slice<'a>(bytes: &'a [u8], start: usize, end: usize, what: &str) -> Result<&'a [u8], String> {
    if start > end {
        return Err(format!("{what} range is reversed"));
    }
    bytes
        .get(start..end)
        .ok_or_else(|| format!("{what} range is outside its object"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(value: usize, width: usize) -> Vec<u8> {
        match width {
            1 => vec![u8::try_from(value).unwrap()],
            2 => u16::try_from(value).unwrap().to_le_bytes().to_vec(),
            _ => u32::try_from(value).unwrap().to_le_bytes().to_vec(),
        }
    }

    fn fixture_hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        assert_eq!(digits.len() % 2, 0);
        digits
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = sha256::Sha256::new();
        hasher.update(bytes);
        sha256::to_base16(&hasher.finalize())
    }

    fn serialized_width(data_len: usize, offsets: usize) -> usize {
        for width in [1, 2, 4] {
            let total = data_len + offsets * width;
            if framing_width(total) == width {
                return width;
            }
        }
        4
    }

    fn tuple(mut fields: Vec<Vec<u8>>, variable: &[usize], alignments: &[usize]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut ends = Vec::new();
        for (index, field) in fields.drain(..).enumerate() {
            let alignment = alignments.get(index).copied().unwrap();
            while data.len() % alignment != 0 {
                data.push(0);
            }
            data.extend(field);
            if variable.contains(&index) && index + 1 != alignments.len() {
                ends.push(data.len());
            }
        }
        let width = serialized_width(data.len(), ends.len());
        for end in ends.into_iter().rev() {
            data.extend(offset(end, width));
        }
        data
    }

    fn array(values: &[Vec<u8>]) -> Vec<u8> {
        array_aligned(values, 1)
    }

    fn array_aligned(values: &[Vec<u8>], alignment: usize) -> Vec<u8> {
        if values.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut ends = Vec::new();
        for value in values {
            while out.len() % alignment != 0 {
                out.push(0);
            }
            out.extend(value);
            ends.push(out.len());
        }
        let width = serialized_width(out.len(), values.len());
        for end in ends {
            out.extend(offset(end, width));
        }
        out
    }

    fn checksum(byte: u8) -> Vec<u8> {
        vec![byte; CHECKSUM_BYTES]
    }

    fn text(value: &str) -> Vec<u8> {
        let mut out = value.as_bytes().to_vec();
        out.push(0);
        out
    }

    fn file(name: &str, byte: u8) -> Vec<u8> {
        tuple(vec![text(name), checksum(byte)], &[0, 1], &[1, 1])
    }

    fn directory(name: &str, tree: u8, meta: u8) -> Vec<u8> {
        tuple(
            vec![text(name), checksum(tree), checksum(meta)],
            &[0, 1, 2],
            &[1, 1, 1],
        )
    }

    fn variant(value: Vec<u8>, value_type: &str) -> Vec<u8> {
        let mut out = value;
        out.push(0);
        out.extend(value_type.as_bytes());
        out
    }

    fn metadata_entry(key: &str, value: Vec<u8>, value_type: &str) -> Vec<u8> {
        tuple(
            vec![text(key), variant(value, value_type)],
            &[0, 1],
            &[1, 8],
        )
    }

    fn commit_with(metadata: Vec<u8>, related: Vec<u8>) -> Vec<u8> {
        tuple(
            vec![
                metadata,
                Vec::new(),
                related,
                text("Firefox 154"),
                text("body"),
                1u64.to_be_bytes().to_vec(),
                checksum(0x11),
                checksum(0x22),
            ],
            &[0, 1, 2, 3, 4, 6, 7],
            &[8, 1, 1, 1, 1, 8, 1, 1],
        )
    }

    fn archive_file(
        size: u64,
        mode: u32,
        rdev: u32,
        target: &str,
        xattrs: Vec<u8>,
        body: &[u8],
    ) -> Vec<u8> {
        archive_file_owned(size, 0, 0, mode, rdev, target, xattrs, body)
    }

    fn archive_file_owned(
        size: u64,
        uid: u32,
        gid: u32,
        mode: u32,
        rdev: u32,
        target: &str,
        xattrs: Vec<u8>,
        body: &[u8],
    ) -> Vec<u8> {
        let header = tuple(
            vec![
                size.to_be_bytes().to_vec(),
                uid.to_be_bytes().to_vec(),
                gid.to_be_bytes().to_vec(),
                mode.to_be_bytes().to_vec(),
                rdev.to_be_bytes().to_vec(),
                text(target),
                xattrs,
            ],
            &[5],
            &[8, 4, 4, 4, 4, 1, 1],
        );
        let mut bytes = u32::try_from(header.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn parses_the_exact_commit_fields_needed_for_traversal() {
        let commit = commit_with(Vec::new(), Vec::new());

        let parsed = parse_commit(&commit).unwrap();

        assert_eq!(parsed.root_tree, Checksum([0x11; CHECKSUM_BYTES]));
        assert_eq!(parsed.root_meta, Checksum([0x22; CHECKSUM_BYTES]));
    }

    #[test]
    fn parses_the_reviewed_commit_metadata_subset() {
        let pair = tuple(
            vec![
                text("org.flathub.Stable"),
                text("app/example/x86_64/stable"),
            ],
            &[0, 1],
            &[1, 1],
        );
        let entries = vec![
            metadata_entry("ostree.collection-binding", text("org.flathub.Stable"), "s"),
            metadata_entry("ostree.collection-refs-binding", array(&[pair]), "a(ss)"),
            metadata_entry(
                "ostree.ref-binding",
                array(&[text("app/example/x86_64/stable")]),
                "as",
            ),
            metadata_entry("xa.ref", text("app/example/x86_64/stable"), "s"),
            metadata_entry("xa.from_commit", text("ancestor"), "s"),
            metadata_entry("xa.download-size", 123u64.to_be_bytes().to_vec(), "t"),
            metadata_entry("xa.installed-size", 456u64.to_be_bytes().to_vec(), "t"),
            metadata_entry(
                "xa.subsets",
                array(&[text("verified"), text("floss")]),
                "as",
            ),
            metadata_entry("xa.metadata", text("[Application]\nname=example\n"), "s"),
        ];
        let commit = commit_with(array_aligned(&entries, 8), Vec::new());

        let parsed = parse_commit(&commit).unwrap();

        assert_eq!(
            parsed.metadata.collection_binding.as_deref(),
            Some("org.flathub.Stable")
        );
        assert_eq!(
            parsed.metadata.collection_refs_binding,
            vec![(
                "org.flathub.Stable".to_string(),
                "app/example/x86_64/stable".to_string()
            )]
        );
        assert_eq!(
            parsed.metadata.ref_binding,
            vec!["app/example/x86_64/stable".to_string()]
        );
        assert_eq!(parsed.metadata.download_size, Some(123));
        assert_eq!(parsed.metadata.installed_size, Some(456));
        assert_eq!(
            parsed.metadata.subsets,
            vec!["verified".to_string(), "floss".to_string()]
        );
    }

    #[test]
    fn refuses_extra_unknown_duplicate_and_mistyped_commit_metadata() {
        let extra = array_aligned(
            &[metadata_entry("xa.extra-data-sources", Vec::new(), "ay")],
            8,
        );
        assert!(parse_commit(&commit_with(extra, Vec::new()))
            .unwrap_err()
            .contains("extra-data-sources"));

        let unknown = array_aligned(&[metadata_entry("vendor.fetch", text("url"), "s")], 8);
        assert!(parse_commit(&commit_with(unknown, Vec::new()))
            .unwrap_err()
            .contains("outside td's reviewed"));

        let duplicate_entry = metadata_entry("xa.ref", text("app/example"), "s");
        let duplicate = array_aligned(&[duplicate_entry.clone(), duplicate_entry], 8);
        assert!(parse_commit(&commit_with(duplicate, Vec::new()))
            .unwrap_err()
            .contains("duplicated"));

        let mistyped = array_aligned(&[metadata_entry("xa.download-size", text("123"), "s")], 8);
        assert!(parse_commit(&commit_with(mistyped, Vec::new()))
            .unwrap_err()
            .contains("expected \"t\""));

        assert!(parse_commit(&commit_with(Vec::new(), vec![1]))
            .unwrap_err()
            .contains("related objects"));
    }

    #[test]
    fn pinned_firefox_objects_are_an_independent_wire_oracle() {
        let commit_bytes = fixture_hex(include_str!(
            "../tests/fixtures/flathub-firefox-154.commit.hex"
        ));
        let expected =
            Checksum::from_hex("86ba63a1c2378a9525b495e1ba2c3ed9dc71ee92f67e45d8016cc4972024b410")
                .unwrap();
        let commit = parse_commit_verified(expected, &commit_bytes).unwrap();
        assert_eq!(
            commit.root_tree.to_hex(),
            "20e1f5dac181295e0e51d3628008003be53ab5d28873507ef10c9d7d28e52524"
        );
        assert_eq!(
            commit.metadata.xa_ref.as_deref(),
            Some("app/org.mozilla.firefox/x86_64/stable")
        );
        assert_eq!(
            commit.metadata.ref_binding,
            vec!["app/org.mozilla.firefox/x86_64/stable".to_string()]
        );
        assert!(commit
            .metadata
            .flatpak_metadata
            .as_deref()
            .is_some_and(|text| text.contains("name=org.mozilla.firefox")));

        let tree_bytes = fixture_hex(include_str!(
            "../tests/fixtures/flathub-firefox-154-root.dirtree.hex"
        ));
        let tree = parse_dirtree_verified(commit.root_tree, &tree_bytes).unwrap();
        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.files[0].name, "metadata");
        assert_eq!(
            tree.files[0].checksum,
            Checksum::from_hex("e2893afcb40e2252a53c4bb1a795d179c7f09dd46d93a165afbfae993dbf0c57")
                .unwrap()
        );
        assert_eq!(
            tree.directories
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["export", "files"]
        );

        let meta_bytes = fixture_hex(include_str!(
            "../tests/fixtures/flathub-firefox-154-root.dirmeta.hex"
        ));
        assert_eq!(
            parse_dirmeta_verified(commit.root_meta, &meta_bytes)
                .unwrap()
                .mode,
            0o040755
        );
    }

    #[test]
    fn pinned_firefox_regular_filez_authenticates_after_raw_inflate() {
        let bytes = fixture_hex(include_str!(
            "../tests/fixtures/flathub-firefox-154-metadata.filez.hex"
        ));
        let expected =
            Checksum::from_hex("e2893afcb40e2252a53c4bb1a795d179c7f09dd46d93a165afbfae993dbf0c57")
                .unwrap();
        assert_eq!(
            sha256_hex(&bytes),
            "5ded1bbcd3337033da61f7274a422804615039605291063b1118e5a3e149abae"
        );

        let decoded = decode_archive_file_verified(expected, &bytes).unwrap();

        assert_eq!(decoded.uid, 0);
        assert_eq!(decoded.gid, 0);
        assert_eq!(decoded.mode, 0o100644);
        let ArchiveFileKind::Regular(contents) = decoded.kind else {
            panic!("Firefox metadata decoded as a symlink");
        };
        assert_eq!(contents.len(), 908);
        let text = std::str::from_utf8(&contents).unwrap();
        assert!(text.starts_with("[Application]\nname=org.mozilla.firefox\n"));
        assert!(text.contains("runtime=org.freedesktop.Platform/x86_64/25.08"));
    }

    #[test]
    fn pinned_firefox_symlink_filez_authenticates_without_a_body() {
        let bytes = fixture_hex(include_str!(
            "../tests/fixtures/flathub-firefox-154-libcanberra-link.filez.hex"
        ));
        let expected =
            Checksum::from_hex("438966c390c22e06217b4ad1919539fe6d2bd2b740416ab19376b34807dfd4eb")
                .unwrap();
        assert_eq!(
            sha256_hex(&bytes),
            "b9de86ad749eb567387080bc76725ecca24160f7ad9ae1ce6d010c8f1643d98a"
        );

        let decoded = decode_archive_file_verified(expected, &bytes).unwrap();

        assert_eq!(decoded.mode, 0o120777);
        assert_eq!(
            decoded.kind,
            ArchiveFileKind::Symlink("libcanberra-gtk3.so.0.1.9".to_string())
        );
    }

    #[test]
    fn archive_filez_refuses_transport_and_content_ambiguity() {
        let fixture = fixture_hex(include_str!(
            "../tests/fixtures/flathub-firefox-154-metadata.filez.hex"
        ));
        let expected =
            Checksum::from_hex("e2893afcb40e2252a53c4bb1a795d179c7f09dd46d93a165afbfae993dbf0c57")
                .unwrap();

        let mut trailing = fixture.clone();
        trailing.push(0);
        assert!(decode_archive_file_verified(expected, &trailing)
            .unwrap_err()
            .contains("consumed"));

        let wrong = Checksum::from_hex(&"00".repeat(CHECKSUM_BYTES)).unwrap();
        assert!(decode_archive_file_verified(wrong, &fixture)
            .unwrap_err()
            .contains("checksum mismatch"));

        let mut bad_padding = fixture.clone();
        bad_padding[4] = 1;
        assert!(decode_archive_file_verified(expected, &bad_padding)
            .unwrap_err()
            .contains("nonzero alignment padding"));

        let header_len = usize::try_from(u32_be(&fixture, 0, "fixture header").unwrap()).unwrap();
        let raw = fixture
            .get(8 + header_len..)
            .expect("pinned archive fixture contains its compressed body");
        let over_declared = archive_file(909, 0o100644, 0, "", Vec::new(), raw);
        assert!(decode_archive_file_verified(expected, &over_declared)
            .unwrap_err()
            .contains("output is 908 bytes; expected 909"));

        let under_declared = archive_file(907, 0o100644, 0, "", Vec::new(), raw);
        assert!(decode_archive_file_verified(expected, &under_declared)
            .unwrap_err()
            .contains("exceeds declared 907 bytes"));
    }

    #[test]
    fn archive_filez_refuses_xattrs_devices_special_bits_and_oversize() {
        let any = Checksum([0; CHECKSUM_BYTES]);
        let empty_raw = [0x03, 0x00];

        let xattrs = archive_file(0, 0o100644, 0, "", vec![1], &empty_raw);
        assert!(decode_archive_file_verified(any, &xattrs)
            .unwrap_err()
            .contains("xattrs"));

        let device = archive_file(0, 0o020644, 1, "", Vec::new(), &[]);
        assert!(decode_archive_file_verified(any, &device)
            .unwrap_err()
            .contains("device number"));

        let fifo = archive_file(0, 0o010644, 0, "", Vec::new(), &[]);
        assert!(decode_archive_file_verified(any, &fifo)
            .unwrap_err()
            .contains("refused file type"));

        let setid = archive_file(0, 0o104644, 0, "", Vec::new(), &empty_raw);
        assert!(decode_archive_file_verified(any, &setid)
            .unwrap_err()
            .contains("setid or sticky"));

        let undefined = archive_file(0, 0x8000_0000 | 0o100644, 0, "", Vec::new(), &empty_raw);
        assert!(decode_archive_file_verified(any, &undefined)
            .unwrap_err()
            .contains("undefined bits"));

        let oversized = archive_file(
            u64::try_from(MAX_ARCHIVE_FILE_BYTES).unwrap() + 1,
            0o100644,
            0,
            "",
            Vec::new(),
            &empty_raw,
        );
        assert!(decode_archive_file_verified(any, &oversized)
            .unwrap_err()
            .contains("limit is"));

        assert!(require_archive_input_size(0).is_err());
        assert!(require_archive_input_size(MAX_ARCHIVE_INPUT_BYTES).is_ok());
        assert!(require_archive_input_size(MAX_ARCHIVE_INPUT_BYTES + 1).is_err());

        let mut oversized_header = vec![0; 8];
        oversized_header[..4].copy_from_slice(
            &u32::try_from(MAX_ARCHIVE_HEADER_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        assert!(decode_archive_file_verified(any, &oversized_header)
            .unwrap_err()
            .contains("archive header size"));
    }

    #[test]
    fn archive_filez_keeps_regular_and_symlink_roles_disjoint() {
        let any = Checksum([0; CHECKSUM_BYTES]);
        let empty_raw = [0x03, 0x00];

        let regular_target = archive_file(0, 0o100644, 0, "target", Vec::new(), &empty_raw);
        assert!(decode_archive_file_verified(any, &regular_target)
            .unwrap_err()
            .contains("regular archive file carries a symlink target"));

        let symlink_size = archive_file(1, 0o120777, 0, "target", Vec::new(), &[]);
        assert!(decode_archive_file_verified(any, &symlink_size)
            .unwrap_err()
            .contains("nonzero content size"));

        let symlink_body = archive_file(0, 0o120777, 0, "target", Vec::new(), &[0]);
        assert!(decode_archive_file_verified(any, &symlink_body)
            .unwrap_err()
            .contains("content body"));

        let long_target = "x".repeat(MAX_SYMLINK_TARGET_BYTES + 1);
        let symlink_target = archive_file(0, 0o120777, 0, &long_target, Vec::new(), &[]);
        assert!(decode_archive_file_verified(any, &symlink_target)
            .unwrap_err()
            .contains("target length"));
    }

    #[test]
    fn canonical_content_header_crosses_the_one_byte_framing_boundary() {
        let one_byte = ArchiveHeader {
            size: 0,
            uid: 1,
            gid: 2,
            mode: 0o120777,
            target: "x".repeat(237),
        };
        let one = canonical_content_header(&one_byte).unwrap();
        assert_eq!(u32_be(&one, 0, "test header length").unwrap(), 255);
        assert_eq!(one.last().copied(), Some(254));

        let two_byte = ArchiveHeader {
            target: "x".repeat(238),
            ..one_byte
        };
        let two = canonical_content_header(&two_byte).unwrap();
        assert_eq!(u32_be(&two, 0, "test header length").unwrap(), 257);
        assert_eq!(two.get(two.len() - 2..), Some([255, 0].as_slice()));
    }

    #[test]
    fn canonical_content_header_pins_nonzero_ownership_and_byte_order() {
        let header = ArchiveHeader {
            size: 0,
            uid: 0x0102_0304,
            gid: 0x1122_3344,
            mode: 0o120777,
            target: "x".to_string(),
        };

        let encoded = canonical_content_header(&header).unwrap();
        assert_eq!(
            encoded,
            fixture_hex("000000130000000001020304112233440000a1ff00000000780012")
        );
    }

    #[test]
    fn archive_decoder_pins_distinct_nonzero_uid_and_gid_offsets() {
        let header = ArchiveHeader {
            size: 0,
            uid: 0x0102_0304,
            gid: 0x1122_3344,
            mode: 0o100644,
            target: String::new(),
        };
        let mut hasher = sha256::Sha256::new();
        hasher.update(&canonical_content_header(&header).unwrap());
        let expected = Checksum(hasher.finalize());
        let bytes = archive_file_owned(
            0,
            header.uid,
            header.gid,
            header.mode,
            0,
            "",
            Vec::new(),
            &[0x03, 0x00],
        );

        let decoded = decode_archive_file_verified(expected, &bytes).unwrap();

        assert_eq!(decoded.uid, header.uid);
        assert_eq!(decoded.gid, header.gid);
        assert_eq!(decoded.kind, ArchiveFileKind::Regular(Vec::new()));
    }

    #[test]
    fn pinned_runtime_commit_uses_the_same_closed_metadata_subset() {
        let bytes = fixture_hex(include_str!(
            "../tests/fixtures/flathub-freedesktop-25.08.commit.hex"
        ));
        let expected =
            Checksum::from_hex("bd44a6230581917d04f89812a4c21090c304d390edb73995af1c2f9fd8abf4e8")
                .unwrap();
        let commit = parse_commit_verified(expected, &bytes).unwrap();

        assert_eq!(
            commit.root_tree.to_hex(),
            "a47b839eb6018af3272ed55f5b27e1cd76f16efaa93cad24561c6c411e889c92"
        );
        assert_eq!(
            commit.root_meta.to_hex(),
            "446a0ef11b7cc167f3b603e585c7eeeeb675faa412d5ec73f62988eb0b6c5488"
        );
        assert_eq!(
            commit.metadata.xa_ref.as_deref(),
            Some("runtime/org.freedesktop.Platform/x86_64/25.08")
        );
        assert_eq!(
            commit.metadata.ref_binding,
            vec!["runtime/org.freedesktop.Platform/x86_64/25.08".to_string()]
        );
    }

    #[test]
    fn parses_sorted_files_and_directories() {
        let files = array(&[file("metadata", 1), file("metadata2", 2)]);
        let directories = array(&[directory("export", 3, 4), directory("files", 5, 6)]);
        let tree = tuple(vec![files, directories], &[0, 1], &[1, 1]);

        let parsed = parse_dirtree(&tree).unwrap();

        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].name, "metadata");
        assert_eq!(parsed.directories.len(), 2);
        assert_eq!(parsed.directories[1].name, "files");
        assert_eq!(parsed.directories[1].tree, Checksum([5; CHECKSUM_BYTES]));
    }

    #[test]
    fn rejects_a_tree_whose_framing_points_into_the_offset_table() {
        let files = array(&[file("metadata", 1)]);
        let mut tree = tuple(vec![files, Vec::new()], &[0, 1], &[1, 1]);
        let last = tree.len() - 1;
        tree[last] = u8::try_from(tree.len()).unwrap();

        let error = parse_dirtree(&tree).unwrap_err();

        assert!(
            error.contains("crosses") || error.contains("framing"),
            "{error}"
        );
    }

    #[test]
    fn array_framing_widths_cross_both_encoding_boundaries() {
        for length in [254usize, 255, 65_533, 65_534] {
            let encoded = array(&[vec![b'x'; length]]);
            let parsed = variable_array(&encoded, 1, "boundary array").unwrap();
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].len(), length);
            assert_eq!(framing_width(encoded.len()), serialized_width(length, 1));
        }
    }

    #[test]
    fn overwide_array_and_tuple_framing_is_not_normal_form() {
        for (length, width) in [(254usize, 2usize), (65_532, 4)] {
            let mut array = vec![b'x'; length];
            array.extend(offset(length, width));
            assert!(variable_array(&array, 1, "overwide array")
                .unwrap_err()
                .contains("non-minimal"));

            let mut tuple = vec![b'x'; length];
            tuple.extend(offset(length, width));
            assert!(tuple_variable_ends(&tuple, 1, "overwide tuple")
                .unwrap_err()
                .contains("non-minimal"));
        }
    }

    #[test]
    fn direct_metadata_count_size_and_name_bounds_are_live() {
        // At this count the smallest `(say)` is a one-byte name plus NUL,
        // checksum, tuple offset, and four-byte array offset: 39 bytes.
        assert!(MAX_TREE_ENTRIES * 39 < MAX_METADATA_BYTES);
        let two = array(&[vec![1], vec![2]]);
        assert!(variable_array(&two, 1, "bounded array")
            .unwrap_err()
            .contains("limit is 1"));

        let oversized = vec![0; MAX_METADATA_BYTES + 1];
        assert!(require_metadata_size(&oversized, "fixture")
            .unwrap_err()
            .contains("size must be"));
        let wrong = Checksum([0; CHECKSUM_BYTES]);
        assert!(parse_commit_verified(wrong, &oversized)
            .unwrap_err()
            .contains("size must be"));
        assert!(parse_dirtree_verified(wrong, &oversized)
            .unwrap_err()
            .contains("size must be"));
        assert!(parse_dirmeta_verified(wrong, &oversized)
            .unwrap_err()
            .contains("size must be"));

        assert_eq!(
            parse_name(&text(&"n".repeat(MAX_NAME_BYTES)))
                .unwrap()
                .len(),
            MAX_NAME_BYTES
        );
        assert!(parse_name(&text(&"n".repeat(MAX_NAME_BYTES + 1)))
            .unwrap_err()
            .contains("length"));
    }

    #[test]
    fn refuses_path_components_and_cross_kind_duplicates() {
        let unsafe_files = array(&[file("../escape", 1)]);
        let unsafe_tree = tuple(vec![unsafe_files, Vec::new()], &[0, 1], &[1, 1]);
        assert!(parse_dirtree(&unsafe_tree).unwrap_err().contains("unsafe"));

        let files = array(&[file("same", 1)]);
        let dirs = array(&[directory("same", 2, 3)]);
        let duplicate = tuple(vec![files, dirs], &[0, 1], &[1, 1]);
        assert!(parse_dirtree(&duplicate)
            .unwrap_err()
            .contains("both a file and a directory"));
    }

    #[test]
    fn dirmeta_accepts_plain_directories_and_refuses_xattrs_or_special_bits() {
        let dirmeta = |mode: u32| {
            let mut bytes = Vec::new();
            bytes.extend(1000u32.to_be_bytes());
            bytes.extend(1000u32.to_be_bytes());
            bytes.extend(mode.to_be_bytes());
            bytes
        };
        let plain = dirmeta(DIRECTORY_TYPE | 0o755);
        assert_eq!(
            parse_dirmeta(&plain).unwrap(),
            Dirmeta {
                uid: 1000,
                gid: 1000,
                mode: DIRECTORY_TYPE | 0o755,
            }
        );

        for bit in [0o1000, 0o2000, 0o4000] {
            assert!(parse_dirmeta(&dirmeta(DIRECTORY_TYPE | 0o755 | bit))
                .unwrap_err()
                .contains("setid or sticky"));
        }

        let xattr = tuple(vec![b"user.fixture".to_vec(), vec![1, 2]], &[0, 1], &[1, 1]);
        let mut xattrs = plain.clone();
        xattrs.extend(array(&[xattr]));
        assert!(parse_dirmeta(&xattrs).unwrap_err().contains("xattrs"));

        let undefined = dirmeta(0x8000_0000 | DIRECTORY_TYPE | 0o755);
        assert!(parse_dirmeta(&undefined)
            .unwrap_err()
            .contains("undefined bits"));
    }

    #[test]
    fn object_identity_is_the_declared_sha256() {
        let bytes = commit_with(Vec::new(), Vec::new());
        let expected = Checksum::from_hex(&sha256::hex_digest(&bytes)).unwrap();
        assert!(parse_commit_verified(expected, &bytes).is_ok());

        let wrong = Checksum([0; CHECKSUM_BYTES]);
        assert!(parse_commit_verified(wrong, &bytes)
            .unwrap_err()
            .contains("checksum mismatch"));
    }
}
