//! Saving what td-web fetched into the human's Downloads (DESIGN.md §5,
//! "Saving"), and only there: in its jail td-web's home is its own
//! account's, and the human's Downloads reaches it only as the grant view
//! td-jail binds at `/home/td/Downloads` (td-web/DESIGN.md §3), so a save
//! there needs that view mounted and writable; on a host, run as the
//! human, it is the human's own `~/Downloads`. A save never overwrites:
//! it creates its file exclusively under the first free name of
//! `NAME`, `NAME (2)` and so on, through a descriptor of the directory
//! opened once without following a link, and writes the whole body, held
//! in memory before the file is made, then syncs it and the directory.
//! Every call it makes is on the shared allowlist (APPLICATIONS.md, "Per-application
//! allowlists"): `openat`, `read`, `write`, `fsync`, `ftruncate`,
//! `statx` and `unlink`; no rename or link, which would want `renameat2`
//! or `linkat`.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use td_url::Url;

/// td-jail's application home (APPLICATIONS.md §C): td-web's own
/// account's, in which `Downloads` is the human's only as a grant view.
pub const JAIL_HOME: &str = "/home/td";
/// What the human runs to let td-web save; it takes effect from td-web's
/// next start, as every grant does.
pub const GRANT: &str = "td-ctl grant application web rw Downloads";
/// The longest name kept, in bytes, before a copy's ` (N)`: well inside
/// Linux's 255.
pub const MAX_NAME: usize = 200;
/// The most names tried: `NAME`, then `NAME (2)` to `NAME (100)`.
pub const MAX_COPIES: u32 = 100;
/// The longest extension kept apart from the name when it is cut or
/// numbered, its dot included.
const MAX_EXTENSION: usize = 16;
/// The most of `/proc/self/mountinfo` read; a jail's is a few KiB.
const MAX_MOUNTINFO: usize = 1 << 20;
/// What the name is when nothing names it.
const FALLBACK: &str = "download";
/// A saved file is the human's alone; every account granted Downloads
/// sees it as its own through its idmapped view.
const MODE: u32 = 0o600;
// x86-64 Linux's open flags; td builds for x86-64 alone.
const O_DIRECTORY: i32 = 0o200_000;
const O_NOFOLLOW: i32 = 0o400_000;

/// Why nothing was saved, or what was left. Said in the status row; its
/// `Display` allocates nothing of its own.
#[derive(Debug)]
pub enum Refusal {
    /// `HOME` is unset, empty or not absolute, so there is no Downloads
    /// to find.
    NoHome,
    /// In the jail, no view is mounted at `/home/td/Downloads`.
    NoGrant,
    /// In the jail, the view is mounted read-only.
    ReadOnly,
    /// `/proc/self/mountinfo` could not be read.
    Mounts(io::Error),
    /// Downloads could not be opened as a directory without a link.
    Directory(io::Error),
    /// No memory for a name or a path.
    Memory,
    /// A name that is no one file's (`.`, `..`, empty or holding `/`),
    /// which `safe` never makes: refused again before it is created.
    Name,
    /// `NAME` and every numbered copy up to `MAX_COPIES` exist.
    Taken,
    /// The file could not be created.
    Create(io::Error),
    /// Writing or syncing failed and the partial file was removed.
    Write(io::Error),
    /// Writing or syncing failed and the partial file, which the name
    /// still names, could not be removed; whether it was emptied first.
    Left {
        why: io::Error,
        name: String,
        emptied: bool,
    },
    /// Writing or syncing failed and the name no longer named the partial
    /// file when td-web came to remove it, or could not be told to: what
    /// the name held then, and whether the file was emptied first.
    Moved {
        why: io::Error,
        name: String,
        emptied: bool,
        now: Named,
    },
}

/// What a partial file's name held when td-web came to remove the file.
#[derive(Debug)]
pub enum Named {
    /// Nothing: something moved or removed the file.
    Nothing,
    /// Another file, which td-web leaves alone.
    Another,
    /// Unknown: the name or the file could not be looked at.
    Unknown(io::Error),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => f.write_str("not saved: HOME is not set to an absolute path"),
            Self::NoGrant => write!(
                f,
                "not saved: td-web has no grant of your Downloads; {GRANT} gives it one \
                 from td-web's next start"
            ),
            Self::ReadOnly => write!(
                f,
                "not saved: td-web's grant of your Downloads is read-only; {GRANT} makes \
                 it writable from td-web's next start"
            ),
            Self::Mounts(why) => write!(f, "not saved: td-web's mounts: {why}"),
            Self::Directory(why) => write!(f, "not saved: Downloads: {why}"),
            Self::Memory => f.write_str("not saved: no memory"),
            Self::Name => f.write_str("not saved: the name is not one file's"),
            Self::Taken => write!(
                f,
                "not saved: Downloads holds the name and {} numbered copies of it",
                MAX_COPIES - 1
            ),
            Self::Create(why) => write!(f, "not saved: {why}"),
            Self::Write(why) => write!(f, "not saved: {why}; the partial file was removed"),
            Self::Left {
                why,
                name,
                emptied: true,
            } => write!(
                f,
                "not saved: {why}; the file was emptied but could not be removed, and is \
                 left in Downloads as {name}"
            ),
            Self::Left {
                why,
                name,
                emptied: false,
            } => write!(
                f,
                "not saved: {why}; the partly written file could be neither emptied nor \
                 removed, and is left in Downloads as {name}"
            ),
            Self::Moved {
                why,
                name,
                emptied,
                now,
            } => {
                let emptied = if *emptied { "emptied" } else { "not emptied" };
                match now {
                    Named::Nothing => write!(
                        f,
                        "not saved: {why}; the file being written, {emptied}, is no longer \
                         at {name}: something moved or removed it"
                    ),
                    Named::Another => write!(
                        f,
                        "not saved: {why}; {name} now names another file, left alone; the \
                         one being written, {emptied}, was moved or removed"
                    ),
                    Named::Unknown(looked) => write!(
                        f,
                        "not saved: {why}; whether {name} is still the file being written, \
                         {emptied}, could not be told ({looked}), so it is left"
                    ),
                }
            }
        }
    }
}

impl From<td_alloc::Error> for Refusal {
    fn from(_: td_alloc::Error) -> Self {
        Self::Memory
    }
}

impl From<td_alloc::FormatError> for Refusal {
    fn from(_: td_alloc::FormatError) -> Self {
        Self::Memory
    }
}

/// Downloads, opened once as a directory without following a link: in
/// td-jail's application home only when the grant view is mounted there
/// writable, else (a host run as the human) `HOME`'s own. A host account
/// whose home is `/home/td` is taken for the jail and saves only into a
/// mount there.
pub fn downloads(home: Option<&Path>) -> Result<File, Refusal> {
    let home = home
        .filter(|home| home.is_absolute())
        .ok_or(Refusal::NoHome)?;
    let mut target = td_alloc::copy(home.as_os_str().as_bytes())?;
    td_alloc::extend_from_slice(&mut target, b"/Downloads")?;
    if home == Path::new(JAIL_HOME) {
        match grant(&mountinfo()?, &target) {
            Grant::None => return Err(Refusal::NoGrant),
            Grant::ReadOnly => return Err(Refusal::ReadOnly),
            Grant::Writable => {}
        }
    }
    let target = PathBuf::from(std::ffi::OsString::from_vec(target));
    OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(target)
        .map_err(Refusal::Directory)
}

/// What is mounted at a path, as this process's mount table says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Grant {
    None,
    ReadOnly,
    Writable,
}

/// This process's mount table, at most `MAX_MOUNTINFO` bytes of it.
fn mountinfo() -> Result<Vec<u8>, Refusal> {
    let mut file = File::open("/proc/self/mountinfo").map_err(Refusal::Mounts)?;
    let mut table = td_alloc::with_capacity(16 << 10)?;
    let mut chunk = [0u8; 8 << 10];
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => return Ok(table),
            Ok(read) => read,
            Err(why) if why.kind() == io::ErrorKind::Interrupted => continue,
            Err(why) => return Err(Refusal::Mounts(why)),
        };
        if table.len() + read > MAX_MOUNTINFO {
            return Err(Refusal::Mounts(io::Error::other(
                "the mount table is too long",
            )));
        }
        td_alloc::extend_from_slice(&mut table, chunk.get(..read).unwrap_or_default())?;
    }
}

/// The one mount at `target` and whether it is read-only: mountinfo's
/// fifth field is the mount point, its sixth the mount's own options. A
/// target holding no space, tab, newline or backslash is written there as
/// it is, as `/home/td/Downloads` is. td-jail refuses a grant mountpoint
/// mountinfo repeats (`grant_mount_rows`), so a target listed twice is no
/// view it made, and no grant.
fn grant(mountinfo: &[u8], target: &[u8]) -> Grant {
    let mut found = Grant::None;
    let mut mounts = 0usize;
    for line in mountinfo.split(|&byte| byte == b'\n') {
        let mut fields = line.split(|&byte| byte == b' ');
        if fields.nth(4) != Some(target) {
            continue;
        }
        let read_only = fields
            .next()
            .unwrap_or_default()
            .split(|&byte| byte == b',')
            .any(|option| option == b"ro");
        mounts += 1;
        found = if read_only {
            Grant::ReadOnly
        } else {
            Grant::Writable
        };
    }
    if mounts == 1 {
        found
    } else {
        Grant::None
    }
}

/// The name a reply is saved under: its `content-disposition`'s filename,
/// else the last segment of its URL's path, percent-decoded, else
/// `download`; each made safe by `safe`.
pub fn name(disposition: Option<&str>, url: &Url) -> Result<String, td_alloc::Error> {
    if let Some(name) = disposition.map(disposition_name).transpose()?.flatten() {
        return Ok(name);
    }
    let text = url.text();
    let path = text.get(url.origin().len()..).unwrap_or_default();
    let path = path.split('?').next().unwrap_or_default();
    let segment = path.rsplit('/').next().unwrap_or_default();
    if let Some(decoded) = percent_decoded(segment.as_bytes())? {
        if let Ok(text) = std::str::from_utf8(&decoded) {
            if let Some(name) = safe(text)? {
                return Ok(name);
            }
        }
    }
    td_alloc::string(FALLBACK)
}

/// The filename a `content-disposition` names (RFC 6266): `filename*`
/// (RFC 8187) in UTF-8 or ISO-8859-1 before `filename`, made safe; none
/// when neither gives a name.
fn disposition_name(value: &str) -> Result<Option<String>, td_alloc::Error> {
    let (mut extended, mut plain) = (None, None);
    // The disposition type comes first and names nothing.
    for parameter in parameters(value).skip(1) {
        let Some((key, raw)) = parameter.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let raw = raw.trim();
        if key.eq_ignore_ascii_case("filename*") && extended.is_none() {
            extended = Some(raw);
        } else if key.eq_ignore_ascii_case("filename") && plain.is_none() {
            plain = Some(raw);
        }
    }
    if let Some(raw) = extended {
        if let Some(name) = extended_value(raw)? {
            return Ok(Some(name));
        }
    }
    match plain {
        Some(raw) => {
            let unquoted = unquoted(raw)?;
            safe(&unquoted)
        }
        None => Ok(None),
    }
}

/// `value`'s parameters, split at each `;` outside a quoted string.
fn parameters(value: &str) -> impl Iterator<Item = &str> {
    let mut quoted = false;
    let mut escaped = false;
    value.split(move |c| {
        if escaped {
            escaped = false;
            return false;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' => return !quoted,
            _ => {}
        }
        false
    })
}

/// A token as it is, or a quoted string's text, its `\` escapes undone.
fn unquoted(raw: &str) -> Result<String, td_alloc::Error> {
    let Some(inner) = raw.strip_prefix('"') else {
        return td_alloc::string(raw);
    };
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, inner.len())?;
    let mut escaped = false;
    let mut bytes = [0u8; 4];
    for c in inner.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
            continue;
        } else if c == '"' {
            break;
        }
        td_alloc::push_str(&mut out, c.encode_utf8(&mut bytes))?;
    }
    Ok(out)
}

/// An RFC 8187 value, `CHARSET'LANGUAGE'PERCENT-ENCODED`, decoded from
/// UTF-8 or ISO-8859-1 and made safe; none for another charset, bytes
/// that are not that charset's, or nothing left.
fn extended_value(raw: &str) -> Result<Option<String>, td_alloc::Error> {
    let mut parts = raw.splitn(3, '\'');
    let (Some(charset), Some(_language), Some(encoded)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Ok(None);
    };
    let Some(bytes) = percent_decoded(encoded.as_bytes())? else {
        return Ok(None);
    };
    if charset.eq_ignore_ascii_case("utf-8") {
        match std::str::from_utf8(&bytes) {
            Ok(text) => safe(text),
            Err(_) => Ok(None),
        }
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        let mut text = String::new();
        td_alloc::reserve_string(&mut text, bytes.len().saturating_mul(2))?;
        let mut buffer = [0u8; 4];
        for &byte in &bytes {
            td_alloc::push_str(&mut text, char::from(byte).encode_utf8(&mut buffer))?;
        }
        safe(&text)
    } else {
        Ok(None)
    }
}

/// `encoded` with each `%XX` made its byte; none when a `%` is not
/// followed by two hexadecimal digits.
fn percent_decoded(encoded: &[u8]) -> Result<Option<Vec<u8>>, td_alloc::Error> {
    let mut out = td_alloc::with_capacity(encoded.len())?;
    let mut at = 0;
    while let Some(&byte) = encoded.get(at) {
        if byte == b'%' {
            let digit = |offset: usize| {
                encoded
                    .get(at + offset)
                    .and_then(|&digit| char::from(digit).to_digit(16))
            };
            let (Some(high), Some(low)) = (digit(1), digit(2)) else {
                return Ok(None);
            };
            td_alloc::push(&mut out, (high * 16 + low) as u8)?;
            at += 3;
        } else {
            td_alloc::push(&mut out, byte)?;
            at += 1;
        }
    }
    Ok(Some(out))
}

/// Whether `c` is left out of a name: a control character (C0, DEL, C1)
/// or a format character that reorders or hides what a name shows, the
/// set td's grant paths refuse (td-authd/DESIGN.md, "Workload grant
/// table").
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// `raw` as a name in one directory: control and format characters left
/// out, `/` and `\` made `_`, dots and spaces dropped from its start in
/// any mix until neither leads (no hidden file, no `.` or `..`), spaces
/// from its end, and at most `MAX_NAME` bytes, cut at a character and
/// keeping a short extension; none when nothing is left. `raw` is bounded
/// already, by the fetch service's 8 KiB head line or by td-url's URL.
fn safe(raw: &str) -> Result<Option<String>, td_alloc::Error> {
    let mut kept = String::new();
    td_alloc::reserve_string(&mut kept, raw.len())?;
    let mut bytes = [0u8; 4];
    for c in raw.chars().filter(|&c| !hidden(c)) {
        let c = if c == '/' || c == '\\' { '_' } else { c };
        td_alloc::push_str(&mut kept, c.encode_utf8(&mut bytes))?;
    }
    let trimmed = kept
        .trim_start_matches(|c: char| c == '.' || c.is_whitespace())
        .trim_end();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() <= MAX_NAME {
        return td_alloc::string(trimmed).map(Some);
    }
    let (stem, extension) = split_extension(trimmed);
    let room = MAX_NAME - extension.len();
    let mut cut = room.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = stem.get(..cut).unwrap_or_default().trim_end();
    if stem.is_empty() {
        return Ok(None);
    }
    let mut out = td_alloc::string(stem)?;
    td_alloc::push_str(&mut out, extension)?;
    Ok(Some(out))
}

/// A name's stem and its extension, the last `.` on: an extension of at
/// most `MAX_EXTENSION` bytes after some stem, else none.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= MAX_EXTENSION => (
            name.get(..dot).unwrap_or(name),
            name.get(dot..).unwrap_or(""),
        ),
        _ => (name, ""),
    }
}

/// The `copy`th name tried for `name`: itself first, then `STEM (N)EXT`.
fn numbered(name: &str, copy: u32) -> Result<String, td_alloc::FormatError> {
    if copy <= 1 {
        return td_alloc::string(name).map_err(|_| td_alloc::FormatError::Allocation);
    }
    let (stem, extension) = split_extension(name);
    td_alloc::format(format_args!("{stem} ({copy}){extension}"))
}

/// A save made: the name it took, and why the directory could not be
/// synced after it, if it could not, the file itself being whole and
/// synced.
#[derive(Debug)]
pub struct Stored {
    pub name: String,
    pub unsynced: Option<io::Error>,
}

/// Saves `body` in `directory` under the first free one of `name`'s
/// names, written whole and synced, then syncs the directory so its new
/// name lasts too.
pub fn store(directory: &File, name: &str, body: &[u8]) -> Result<Stored, Refusal> {
    let name = store_with(directory, name, body, |file, body| {
        file.write_all(body)?;
        file.sync_all()
    })?;
    let unsynced = directory.sync_all().err();
    Ok(Stored { name, unsynced })
}

/// `store`, writing with `write`. The file is created exclusively, so
/// neither an existing file nor a link in its place is touched, through
/// `/proc/self/fd`, so it is made in the directory opened, whatever its
/// path names now. While it is written it is visible under its final
/// name; a write that fails is undone by `discard`. A name that is not
/// one file's is refused, though `safe` never makes one.
fn store_with(
    directory: &File,
    name: &str,
    body: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> Result<String, Refusal> {
    let descriptor = directory.as_raw_fd();
    for copy in 1..=MAX_COPIES {
        let candidate = numbered(name, copy)?;
        if matches!(candidate.as_str(), "" | "." | "..") || candidate.contains('/') {
            return Err(Refusal::Name);
        }
        let path = td_alloc::format(format_args!("/proc/self/fd/{descriptor}/{candidate}"))?;
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(MODE)
            .custom_flags(O_NOFOLLOW)
            .open(&path);
        let mut file = match created {
            Ok(file) => file,
            Err(why) if why.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(why) => return Err(Refusal::Create(why)),
        };
        return match write(&mut file, body) {
            Ok(()) => Ok(candidate),
            Err(why) => Err(discard(file, Path::new(&path), why, candidate)),
        };
    }
    Err(Refusal::Taken)
}

/// A partial file after `why`: emptied through its own descriptor, which
/// a full disk may refuse, then removed whether or not it was emptied,
/// while `path` still names it, the descriptor held open until then so
/// its inode cannot be another's. The look and the unlink are two steps:
/// another writer to Downloads can put a file at the name between them,
/// which only an unlink by descriptor would close, and Linux has none.
fn discard(file: File, path: &Path, why: io::Error, name: String) -> Refusal {
    let emptied = file.set_len(0).is_ok();
    let now = match (file.metadata(), std::fs::symlink_metadata(path)) {
        (Ok(ours), Ok(named)) if ours.dev() == named.dev() && ours.ino() == named.ino() => None,
        (Ok(_), Ok(_)) => Some(Named::Another),
        (Ok(_), Err(looked)) if looked.kind() == io::ErrorKind::NotFound => Some(Named::Nothing),
        (Err(looked), _) | (_, Err(looked)) => Some(Named::Unknown(looked)),
    };
    let Some(now) = now else {
        let removed = std::fs::remove_file(path).is_ok();
        drop(file);
        return if removed {
            Refusal::Write(why)
        } else {
            Refusal::Left { why, name, emptied }
        };
    };
    drop(file);
    Refusal::Moved {
        why,
        name,
        emptied,
        now,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use td_fs::TempDir;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn named(disposition: Option<&str>, at: &str) -> String {
        name(disposition, &url(at)).unwrap()
    }

    #[test]
    fn a_disposition_names_the_file_before_the_url() {
        let at = "https://example.org/dl/get.php?id=7";
        assert_eq!(
            named(Some("attachment; filename=report.pdf"), at),
            "report.pdf"
        );
        assert_eq!(
            named(
                Some("attachment; filename=\"Q3 \\\"final\\\"; v2.pdf\""),
                at
            ),
            "Q3 \"final\"; v2.pdf",
            "a quoted string keeps its ; and its escaped quotes"
        );
        assert_eq!(
            named(
                Some(
                    "attachment; filename=\"fallback.txt\"; filename*=UTF-8''%E2%82%AC%20rates.txt"
                ),
                at
            ),
            "€ rates.txt",
            "filename* before filename"
        );
        assert_eq!(
            named(Some("attachment; FILENAME*=iso-8859-1'en'caf%E9.txt"), at),
            "café.txt"
        );
        assert_eq!(
            named(Some("inline"), at),
            "get.php",
            "the URL's last segment"
        );
        assert_eq!(
            named(
                Some("attachment; filename*=UTF-8''%FF%FE.txt; filename=ok.txt"),
                at
            ),
            "ok.txt",
            "bytes that are not UTF-8 fall back to filename"
        );
        assert_eq!(
            named(Some("attachment; filename*=koi8-r''x.txt"), at),
            "get.php"
        );
        assert_eq!(
            named(None, "https://example.org/a%20b/r%C3%A9sum%C3%A9.pdf"),
            "résumé.pdf"
        );
        assert_eq!(named(None, "https://example.org/"), "download");
        assert_eq!(named(None, "https://example.org/%ZZ"), "download");
        assert_eq!(named(None, "https://example.org/%FF"), "download");
    }

    #[test]
    fn a_hostile_disposition_names_one_plain_file() {
        let at = "https://example.org/x";
        let cases: &[(&str, &str)] = &[
            ("attachment; filename=\"../../.bashrc\"", "_.._.bashrc"),
            ("attachment; filename=\"/etc/passwd\"", "_etc_passwd"),
            (
                "attachment; filename=\"..\\\\..\\\\win.ini\"",
                "_.._win.ini",
            ),
            ("attachment; filename=\".hidden\"", "hidden"),
            ("attachment; filename=\"...\"", "x"),
            ("attachment; filename=\"   \"", "x"),
            ("attachment; filename*=UTF-8''a%00b%0Ac%1B.txt", "abc.txt"),
            (
                "attachment; filename*=UTF-8''invoice%E2%80%AEfdp.exe",
                "invoicefdp.exe",
            ),
            ("attachment; filename*=UTF-8''%E2%80%8B%EF%BB%BFok", "ok"),
            ("attachment; filename*=UTF-8''%C2%85x%7F", "x"),
            ("attachment; filename*=UTF-8''%2E%2E%2Fup", "_up"),
            // Dots and spaces in any mix lead nothing.
            ("attachment; filename=\". .bashrc\"", "bashrc"),
            ("attachment; filename=\". ..\"", "x"),
            ("attachment; filename=\". .\"", "x"),
            ("attachment; filename*=UTF-8''.%C2%A0.profile", "profile"),
            (
                "attachment; filename*=UTF-8''%2E%E3%80%80%2Ehidden",
                "hidden",
            ),
            ("attachment; filename=\"  . \t. x. \"", "x."),
        ];
        for (disposition, expected) in cases {
            assert_eq!(&named(Some(disposition), at), expected, "{disposition}");
        }
        // The URL's segment takes the same rule.
        for at in [
            "https://example.org/a/.%20..",
            "https://example.org/a/.%20.",
            "https://example.org/a/%20.%E3%80%80",
        ] {
            assert_eq!(named(None, at), "download", "{at}");
        }
        assert_eq!(named(None, "https://example.org/a/.%20.x"), "x");
    }

    #[test]
    fn a_long_name_is_cut_at_a_character_keeping_its_extension() {
        let long = format!("attachment; filename=\"{}.tar.gz\"", "é".repeat(300));
        let name = named(Some(&long), "https://example.org/x");
        assert!(name.len() <= MAX_NAME, "{}", name.len());
        assert!(name.ends_with("é.gz"), "{name}");
        // An extension past MAX_EXTENSION is cut with the rest.
        let long = format!(
            "attachment; filename=\"{}.{}\"",
            "a".repeat(250),
            "b".repeat(40)
        );
        let name = named(Some(&long), "https://example.org/x");
        assert_eq!(name, "a".repeat(MAX_NAME));
        // However long, the name keeps its extension.
        let long = format!("attachment; filename=\"{}.pdf\"", "€".repeat(2000));
        let name = named(Some(&long), "https://example.org/x");
        assert!(name.len() <= MAX_NAME && name.ends_with("€.pdf"), "{name}");
    }

    #[test]
    fn copies_are_numbered_before_the_extension() {
        assert_eq!(numbered("a.pdf", 1).unwrap(), "a.pdf");
        assert_eq!(numbered("a.pdf", 2).unwrap(), "a (2).pdf");
        assert_eq!(numbered("archive.tar.gz", 3).unwrap(), "archive.tar (3).gz");
        assert_eq!(numbered("README", 2).unwrap(), "README (2)");
        assert_eq!(numbered("download", 100).unwrap(), "download (100)");
    }

    fn directory(temp: &TempDir) -> File {
        File::open(temp.path()).unwrap()
    }

    #[test]
    fn a_save_takes_the_first_free_name_and_overwrites_nothing() {
        let temp = TempDir::new("td-web-save").unwrap();
        std::fs::write(temp.join("a.pdf"), b"theirs").unwrap();
        std::fs::write(temp.join("a (2).pdf"), b"theirs too").unwrap();
        let dir = directory(&temp);
        assert_eq!(store(&dir, "a.pdf", b"mine").unwrap().name, "a (3).pdf");
        assert_eq!(std::fs::read(temp.join("a.pdf")).unwrap(), b"theirs");
        assert_eq!(
            std::fs::read(temp.join("a (2).pdf")).unwrap(),
            b"theirs too"
        );
        assert_eq!(std::fs::read(temp.join("a (3).pdf")).unwrap(), b"mine");
        let mode = std::fs::metadata(temp.join("a (3).pdf"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, MODE);
        assert_eq!(
            store(&dir, "b", b"").unwrap().name,
            "b",
            "an empty body saves"
        );
        temp.cleanup().unwrap();
    }

    #[test]
    fn once_every_copy_is_taken_nothing_is_saved() {
        let temp = TempDir::new("td-web-save").unwrap();
        for copy in 1..=MAX_COPIES {
            std::fs::write(temp.join(numbered("n.txt", copy).unwrap()), b"x").unwrap();
        }
        let dir = directory(&temp);
        assert!(matches!(store(&dir, "n.txt", b"mine"), Err(Refusal::Taken)));
        temp.cleanup().unwrap();
    }

    #[test]
    fn a_link_in_downloads_is_never_followed() {
        let temp = TempDir::new("td-web-save").unwrap();
        let downloads = temp.join("Downloads");
        std::fs::create_dir(&downloads).unwrap();
        let elsewhere = temp.join("elsewhere");
        std::fs::write(&elsewhere, b"precious").unwrap();
        // A link to a file, and a dangling one a create would follow.
        symlink(&elsewhere, downloads.join("x.txt")).unwrap();
        symlink(temp.join("made"), downloads.join("x (2).txt")).unwrap();
        let dir = File::open(&downloads).unwrap();
        assert_eq!(store(&dir, "x.txt", b"mine").unwrap().name, "x (3).txt");
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"precious");
        assert!(!temp.join("made").exists());
        // Downloads itself as a link is refused, not followed.
        let home = temp.join("home");
        std::fs::create_dir(&home).unwrap();
        symlink(&downloads, home.join("Downloads")).unwrap();
        assert!(matches!(downloads_of(&home), Err(Refusal::Directory(_))));
        temp.cleanup().unwrap();
    }

    /// The directory a save opened, even once its path names another.
    #[test]
    fn a_save_writes_in_the_directory_it_opened() {
        let temp = TempDir::new("td-web-save").unwrap();
        let home = temp.join("home");
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        let dir = downloads_of(&home).unwrap();
        std::fs::rename(home.join("Downloads"), temp.join("moved")).unwrap();
        symlink(temp.join("other"), home.join("Downloads")).unwrap();
        assert_eq!(store(&dir, "f", b"1").unwrap().name, "f");
        assert_eq!(std::fs::read(temp.join("moved/f")).unwrap(), b"1");
        temp.cleanup().unwrap();
    }

    fn downloads_of(home: &Path) -> Result<File, Refusal> {
        downloads(Some(home))
    }

    #[test]
    fn a_host_home_saves_into_its_own_downloads() {
        let temp = TempDir::new("td-web-save").unwrap();
        assert!(matches!(downloads(None), Err(Refusal::NoHome)));
        for home in ["", "home", "./x"] {
            let refused = downloads(Some(Path::new(home)));
            assert!(matches!(refused, Err(Refusal::NoHome)), "{home:?}");
        }
        let missing = downloads_of(&temp);
        assert!(matches!(missing, Err(Refusal::Directory(_))), "{missing:?}");
        std::fs::write(temp.join("Downloads"), b"").unwrap();
        assert!(matches!(downloads_of(&temp), Err(Refusal::Directory(_))));
        temp.cleanup().unwrap();
    }

    #[test]
    fn a_write_that_fails_leaves_no_partial_file() {
        let temp = TempDir::new("td-web-save").unwrap();
        let dir = directory(&temp);
        let failed = store_with(&dir, "big.bin", b"abcdef", |file, body| {
            file.write_all(&body[..3])?;
            Err(io::Error::other("the disk is full"))
        });
        let Err(Refusal::Write(why)) = failed else {
            panic!("{failed:?}");
        };
        assert_eq!(why.to_string(), "the disk is full");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        // A name that no longer names the partial file is left alone; the
        // file it named was emptied through its own descriptor.
        let other = temp.join("elsewhere");
        let failed = store_with(&dir, "big.bin", b"abcdef", |file, body| {
            file.write_all(body)?;
            std::fs::rename(temp.join("big.bin"), &other)?;
            std::fs::write(temp.join("big.bin"), b"theirs")?;
            Err(io::Error::other("the disk is full"))
        });
        let Err(Refusal::Moved {
            name,
            emptied: true,
            now: Named::Another,
            ..
        }) = &failed
        else {
            panic!("{failed:?}");
        };
        assert_eq!(name, "big.bin");
        let said = failed.unwrap_err().to_string();
        assert!(said.contains("big.bin now names another file"), "{said}");
        assert!(!said.contains("left in Downloads as big.bin"), "{said}");
        assert_eq!(std::fs::read(temp.join("big.bin")).unwrap(), b"theirs");
        assert_eq!(std::fs::read(&other).unwrap(), b"");
        // A name that names nothing any more: the file was removed.
        std::fs::remove_file(temp.join("big.bin")).unwrap();
        std::fs::remove_file(&other).unwrap();
        let failed = store_with(&dir, "big.bin", b"abcdef", |file, body| {
            file.write_all(body)?;
            std::fs::remove_file(temp.join("big.bin"))?;
            Err(io::Error::other("the disk is full"))
        });
        let Err(Refusal::Moved {
            now: Named::Nothing,
            ..
        }) = &failed
        else {
            panic!("{failed:?}");
        };
        let said = failed.unwrap_err().to_string();
        assert!(said.contains("is no longer at big.bin"), "{said}");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        temp.cleanup().unwrap();
    }

    /// A full disk may refuse the truncate too: the file is removed all
    /// the same while its name names it.
    #[test]
    fn a_partial_file_is_removed_though_it_could_not_be_emptied() {
        let temp = TempDir::new("td-web-save").unwrap();
        let path = temp.join("part");
        std::fs::write(&path, b"abc").unwrap();
        // Open for reading alone, so emptying it through this descriptor
        // fails as a refused truncate would.
        let file = File::open(&path).unwrap();
        let why = io::Error::other("the disk is full");
        let refusal = discard(file, &path, why, "part".into());
        assert!(matches!(refusal, Refusal::Write(_)), "{refusal:?}");
        assert!(!path.exists());
        temp.cleanup().unwrap();
    }

    #[test]
    fn a_name_that_is_not_one_file_s_is_refused_before_it_is_made() {
        let temp = TempDir::new("td-web-save").unwrap();
        let dir = directory(&temp);
        for name in ["", ".", "..", "a/b"] {
            let refused = store(&dir, name, b"x");
            assert!(
                matches!(refused, Err(Refusal::Name)),
                "{name:?}: {refused:?}"
            );
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        temp.cleanup().unwrap();
    }

    const MOUNTS: &str = "\
22 1 0:20 / / ro,nosuid,nodev - tmpfs tmpfs ro\n\
30 22 0:31 /@var/lib/td/applications/65540 /home/td rw,nosuid,nodev - btrfs /dev/vda2 rw\n";

    #[test]
    fn in_the_jail_only_a_writable_view_is_downloads() {
        let target = b"/home/td/Downloads";
        assert_eq!(grant(MOUNTS.as_bytes(), target), Grant::None);
        let writable = format!(
            "{MOUNTS}41 30 0:31 /@var/home/human/Downloads /home/td/Downloads \
             rw,nosuid,nodev,noexec,relatime,idmapped - btrfs /dev/vda2 rw\n"
        );
        assert_eq!(grant(writable.as_bytes(), target), Grant::Writable);
        let read_only = writable.replace("Downloads rw,", "Downloads ro,");
        assert_eq!(grant(read_only.as_bytes(), target), Grant::ReadOnly);
        // A directory below the home, or a mount elsewhere, is no grant.
        let below = format!("{MOUNTS}41 30 0:31 /@var/home/human/Downloads /home/td/Downloads/x rw - btrfs /dev/vda2 rw\n");
        assert_eq!(grant(below.as_bytes(), target), Grant::None);
        // A target listed twice is no view td-jail made.
        let stacked =
            format!("{writable}42 41 0:32 / /home/td/Downloads ro,nosuid - tmpfs tmpfs ro\n");
        assert_eq!(grant(stacked.as_bytes(), target), Grant::None);
    }

    /// In td-jail's home a directory td-web could have made itself is
    /// not the human's Downloads: this process's own mount table has no
    /// view there.
    #[test]
    fn the_jail_home_needs_a_view() {
        let mounted = mountinfo().unwrap();
        if grant(&mounted, b"/home/td/Downloads") == Grant::None {
            let refused = downloads(Some(Path::new(JAIL_HOME)));
            assert!(matches!(refused, Err(Refusal::NoGrant)), "{refused:?}");
            let said = td_alloc::format(format_args!("{}", Refusal::NoGrant)).unwrap();
            assert!(said.contains(GRANT), "{said}");
        }
        let said = td_alloc::format(format_args!("{}", Refusal::ReadOnly)).unwrap();
        assert!(said.contains("read-only") && said.contains(GRANT), "{said}");
    }
}
