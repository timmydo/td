//! Attaching a file to a draft: the folders the compose view's finder
//! lists, and the chosen file copied into the draft's attachment sidecar
//! with the MML `<#part>` tag that names the copy. The finder lists what
//! this process can read, so in the jail what its grants show (the state
//! directory and Downloads) and on a host everything; the copy is what
//! the send reads and what retires with the draft to `sent`, so a file
//! changed or removed after it was attached does not change the message.

use crate::compose;
use std::collections::BinaryHeap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use td_ui::finder;

/// The most a file attached here may hold: the fetch service's request
/// bound, which an upload cannot pass; the server's own `maxSizeUpload`
/// is checked when the draft is sent.
pub const CEILING: u64 = crate::td_fetch::MAX_REQUEST_BODY;

/// The folder the finder opens on when none was chosen from before:
/// `$HOME` when it is an absolute folder, else the root.
pub fn start_folder() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The most directory entries one listing examines, hidden and odd
/// ones included, so a folder of very many is read to a bound, not whole.
pub const EXAMINED: usize = 16 * finder::ENTRIES;

/// A folder for the finder: its subfolders and regular files as a
/// listing, folders first, each sorted with case aside. What is shown is
/// the first `finder::ENTRIES` in that order of those among the first
/// `EXAMINED` entries read, and the first `finder::LISTING_BYTES` of
/// their text, so it does not hang on the order the folder is read in;
/// the listing says it was cut short when an entry that would be listed
/// was left out or the read stopped at `EXAMINED`. A link is followed to learn
/// what it is, a folder shown marked `link`; a file shows its size and
/// is disabled past `ceiling`. A name beginning `.` unless `hidden`, an
/// entry that is neither a folder nor a regular file (a pipe, a socket,
/// a device), one gone or unreadable between the read and its type, and
/// a name that is not text or that the finder cannot show, are left out.
pub fn list_folder(path: &Path, ceiling: u64, hidden: bool) -> Result<finder::Listing, String> {
    let named = |e: io::Error| format!("{}: {e}", path.display());
    // (folders before files, the name with case aside, the name, the
    // size, a link): a max-heap keeps the first ENTRIES in that order.
    let mut kept: BinaryHeap<(bool, String, String, u64, bool)> = BinaryHeap::new();
    let mut truncated = false;
    for (seen, entry) in fs::read_dir(path).map_err(named)?.enumerate() {
        if seen >= EXAMINED {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // A hidden name unless asked for, and one the finder would
        // refuse, take no place.
        if (!hidden && name.starts_with('.'))
            || name.len() > finder::NAME_BYTES
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let metadata = if kind.is_symlink() {
            fs::metadata(entry.path())
        } else {
            entry.metadata()
        };
        let Ok(metadata) = metadata else {
            continue;
        };
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        kept.push((
            metadata.is_file(),
            name.to_lowercase(),
            name,
            metadata.len(),
            kind.is_symlink(),
        ));
        if kept.len() > finder::ENTRIES {
            kept.pop();
            truncated = true;
        }
    }
    let mut entries = Vec::with_capacity(kept.len());
    let mut bytes = 0usize;
    let candidates = kept
        .into_sorted_vec()
        .into_iter()
        .map(|(file, _, name, size, link)| {
            if file {
                finder::Entry::new(&name, &size_text(size), finder::Kind::File, size <= ceiling)
            } else {
                finder::Entry::new(
                    &name,
                    if link { "link" } else { "" },
                    finder::Kind::Folder,
                    true,
                )
            }
        });
    for entry in candidates.flatten() {
        let next = bytes.saturating_add(entry.name().len() + entry.meta().len());
        if next > finder::LISTING_BYTES {
            truncated = true;
            break;
        }
        bytes = next;
        entries.push(entry);
    }
    finder::Listing::new(&path.to_string_lossy(), entries, truncated)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A size as a row's meta: bytes, or the largest binary unit it reaches,
/// to a tenth below ten of it.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut unit = 0;
    let mut scale = 1u64;
    while unit + 1 < UNITS.len() && bytes >= scale.saturating_mul(1024) {
        scale = scale.saturating_mul(1024);
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    let tenths = u128::from(bytes) * 10 / u128::from(scale);
    if unit > 0 && tenths < 100 {
        format!("{}.{} {name}", tenths / 10, tenths % 10)
    } else {
        format!("{} {name}", bytes / scale)
    }
}

/// The media type a file's name suggests, by its extension, with case
/// aside; `application/octet-stream` for one not known.
pub fn media_type(name: &str) -> &'static str {
    let extension = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
        _ => return "application/octet-stream",
    };
    match extension.as_str() {
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "ics" => "text/calendar",
        "vcf" => "text/vcard",
        "eml" => "message/rfc822",
        "xml" => "application/xml",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "rtf" => "application/rtf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// A file attached: the sidecar it was copied into and whether this
/// made it, the copy's path, the name the recipient sees, its size, and
/// the tag naming it, to go into the draft.
#[derive(Clone, Debug)]
pub struct Attached {
    pub sidecar: PathBuf,
    pub created: bool,
    pub path: PathBuf,
    pub name: String,
    pub bytes: usize,
    pub tag: String,
}

/// The sidecar a draft's attachments go in: `td-mail-att-ID` beside
/// `td-mail-draft-ID.eml`, as `compose` names the pair.
pub fn sidecar_for(draft: &Path) -> io::Result<PathBuf> {
    let dir = draft
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| invalid("the draft path has no directory"))?;
    let id = draft
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("td-mail-draft-"))
        .and_then(|name| name.strip_suffix(".eml"))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("the draft is not named td-mail-draft-ID.eml"))?;
    Ok(dir.join(format!("td-mail-att-{id}")))
}

/// Copies the regular file at `source` into the draft's sidecar and
/// answers the tag that attaches the copy. The sidecar is `sidecar` when
/// the draft has one, which must be beside it (what `retire_draft`
/// moves), else `sidecar_for`'s, made private; one that is not a private
/// folder is refused. The file is read as the send reads it (not
/// blocking, a regular file, at most `ceiling` bytes, the file opened the
/// one read) before anything is made; the copy keeps the file's name,
/// cleaned to one path component MML can carry, with `-2`, `-3`, … before
/// the extension when the name is taken, the tag then naming it for the
/// recipient as the file was. What this call made is removed again when
/// it fails.
pub fn attach_file(
    draft: &Path,
    sidecar: Option<&Path>,
    source: &Path,
    ceiling: u64,
) -> io::Result<Attached> {
    let sidecar = match sidecar {
        Some(dir) => {
            if dir.parent() != draft.parent() {
                return Err(invalid("the draft's attachment folder is not beside it"));
            }
            dir.to_path_buf()
        }
        None => sidecar_for(draft)?,
    };
    if sidecar
        .to_str()
        .is_none_or(|text| !compose::valid_mml_attribute(text))
    {
        return Err(invalid(
            "the attachment folder cannot be named in the draft's MML",
        ));
    }
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .map(clean_name)
        .ok_or_else(|| invalid("the file's name is not text"))?;
    let bytes = crate::submit::read_regular(source, ceiling).map_err(io::Error::other)?;

    let created = match fs::DirBuilder::new().mode(0o700).create(&sidecar) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e),
    };
    let private = fs::symlink_metadata(&sidecar)
        .is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o077 == 0);
    if !private {
        if created {
            let _ = fs::remove_dir(&sidecar);
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be a private folder, not a link", sidecar.display()),
        ));
    }

    let mut claimed = None;
    let result: io::Result<(PathBuf, String)> = (|| {
        let (path, mut file) = claim(&sidecar, &name)?;
        claimed = Some(path.clone());
        file.write_all(&bytes)?;
        file.sync_all()?;
        let shown = path
            .file_name()
            .and_then(|shown| shown.to_str())
            .unwrap_or_default();
        let renamed = (shown != name).then_some(name.as_str());
        let tag = compose::mml_part(media_type(&name), &path, renamed, None)?;
        Ok((path, tag))
    })();
    match result {
        Ok((path, tag)) => Ok(Attached {
            sidecar,
            created,
            path,
            name,
            bytes: bytes.len(),
            tag,
        }),
        Err(error) => {
            let mut detail = error.to_string();
            if let Some(path) = &claimed {
                if let Err(e) = fs::remove_file(path) {
                    detail.push_str(&format!("; {} remains: {e}", path.display()));
                }
            }
            if created {
                if let Err(e) = fs::remove_dir(&sidecar) {
                    detail.push_str(&format!("; {} remains: {e}", sidecar.display()));
                }
            }
            Err(io::Error::new(error.kind(), detail))
        }
    }
}

/// A file's name as one path component an MML attribute carries: a
/// separator, a quote, a backslash, an angle bracket, a control
/// character or a bidirectional control (which would show the recipient
/// another extension than the name has) is `_`, leading and trailing
/// dots and spaces are dropped, and a name that leaves nothing is
/// `attachment`.
fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '"' | '<' | '>' => '_',
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' => '_',
            '\u{2066}'..='\u{2069}' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']);
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned.to_string()
    }
}

/// The most names tried for one copy before the sidecar is called full.
const NAME_TRIES: usize = 1000;

/// The bytes of one path component Linux takes.
const NAME_MAX: usize = 255;

/// A new private file in `dir` under `name`, or under `name` with `-2`,
/// `-3`, … before its extension when that is taken, the stem shortened
/// so the name stays one component long where the extension leaves room.
fn claim(dir: &Path, name: &str) -> io::Result<(PathBuf, fs::File)> {
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    for n in 1..=NAME_TRIES {
        let candidate = if n == 1 {
            name.to_string()
        } else {
            let suffix = match extension {
                Some(extension) => format!("-{n}.{extension}"),
                None => format!("-{n}"),
            };
            let mut room = NAME_MAX.saturating_sub(suffix.len()).min(stem.len());
            while !stem.is_char_boundary(room) {
                room -= 1;
            }
            format!("{}{suffix}", stem.get(..room).unwrap_or_default())
        };
        let path = dir.join(&candidate);
        match compose::create_secure_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "{} already holds {NAME_TRIES} files named {name}",
            dir.display()
        ),
    ))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "td-mail-attach-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    #[test]
    fn a_folder_lists_its_folders_then_its_files_the_hidden_and_the_odd_left_out() {
        let dir = scratch("list");
        fs::create_dir(dir.join("beta")).unwrap();
        fs::create_dir(dir.join("Alpha")).unwrap();
        fs::create_dir(dir.join(".hidden")).unwrap();
        fs::write(dir.join("b.txt"), b"hello").unwrap();
        fs::write(dir.join("A.pdf"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("big.bin"), vec![0u8; 101]).unwrap();
        fs::write(dir.join(".secret"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("beta"), dir.join("to-beta")).unwrap();
        std::os::unix::fs::symlink(dir.join("b.txt"), dir.join("to-b")).unwrap();
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
        // A socket is left out; a scratch path too long to bind one
        // leaves the case untried rather than the test failed.
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("sock"));

        let listing = list_folder(&dir, 100, false).unwrap();
        assert_eq!(listing.path(), dir.to_string_lossy());
        assert!(!listing.truncated());
        let shown: Vec<(&str, &str, finder::Kind, bool)> = listing
            .entries()
            .iter()
            .map(|e| (e.name(), e.meta(), e.kind(), e.enabled()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("Alpha", "", finder::Kind::Folder, true),
                ("beta", "", finder::Kind::Folder, true),
                ("to-beta", "link", finder::Kind::Folder, true),
                ("A.pdf", "2.0 KiB", finder::Kind::File, false),
                ("b.txt", "5 B", finder::Kind::File, true),
                ("big.bin", "101 B", finder::Kind::File, false),
                ("to-b", "5 B", finder::Kind::File, true),
            ]
        );
        assert!(list_folder(&dir.join("missing"), 100, false)
            .unwrap_err()
            .contains("missing"));
        // Asked for, the hidden are listed among the rest, in the same
        // order.
        let names: Vec<String> = list_folder(&dir, 100, true)
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                ".hidden", "Alpha", "beta", "to-beta", ".secret", "A.pdf", "b.txt", "big.bin",
                "to-b"
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A folder of more entries than the finder shows lists the first in
    /// sorted order, whatever order the folder reads in, and says it was
    /// cut short.
    #[test]
    fn a_folder_past_the_bound_lists_its_first_entries_in_order() {
        let dir = scratch("bound");
        for n in (0..=finder::ENTRIES).rev() {
            fs::write(dir.join(format!("f{n:05}")), b"").unwrap();
        }
        fs::create_dir(dir.join("z-folder")).unwrap();
        let listing = list_folder(&dir, 100, false).unwrap();
        assert!(listing.truncated());
        let names: Vec<&str> = listing.entries().iter().map(|e| e.name()).collect();
        assert_eq!(names.len(), finder::ENTRIES);
        assert_eq!(names.first(), Some(&"z-folder"), "folders first");
        assert_eq!(names.get(1), Some(&"f00000"));
        assert_eq!(
            names.last().copied(),
            Some(format!("f{:05}", finder::ENTRIES - 2).as_str())
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_and_media_types_read_as_a_person_would() {
        assert_eq!(size_text(0), "0 B");
        assert_eq!(size_text(1023), "1023 B");
        assert_eq!(size_text(1024), "1.0 KiB");
        assert_eq!(size_text(1536), "1.5 KiB");
        assert_eq!(size_text(10 * 1024), "10 KiB");
        assert_eq!(size_text(32 * 1024 * 1024), "32 MiB");
        assert_eq!(size_text(u64::MAX), "17179869183 GiB");
        assert!(size_text(u64::MAX).len() <= finder::META_BYTES);
        assert_eq!(media_type("report.PDF"), "application/pdf");
        assert_eq!(media_type("a.tar.gz"), "application/gzip");
        assert_eq!(media_type("photo.jpeg"), "image/jpeg");
        assert_eq!(media_type("notes"), "application/octet-stream");
        assert_eq!(media_type(".txt"), "application/octet-stream");
        assert_eq!(media_type("x.unknown"), "application/octet-stream");
    }

    #[test]
    fn a_file_is_copied_into_the_drafts_private_sidecar_and_its_tag_sends() {
        let dir = scratch("copy");
        let drafts = dir.join("drafts");
        fs::DirBuilder::new().mode(0o700).create(&drafts).unwrap();
        let draft = drafts.join("td-mail-draft-7-8.eml");
        fs::write(&draft, "From: me@example.com\nTo: you@example.com\nSubject: s\n--text follows this line--\nhi\n").unwrap();
        let source = dir.join("report.pdf");
        fs::write(&source, b"%PDF-1").unwrap();

        let first = attach_file(&draft, None, &source, 100).unwrap();
        let sidecar = drafts.join("td-mail-att-7-8");
        assert_eq!(first.sidecar, sidecar);
        assert_eq!(first.path, sidecar.join("report.pdf"));
        assert_eq!((first.name.as_str(), first.bytes), ("report.pdf", 6));
        assert_eq!(fs::read(&first.path).unwrap(), b"%PDF-1");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&sidecar), 0o700);
        assert_eq!(mode(&first.path), 0o600);
        assert!(first.tag.contains("type=\"application/pdf\""));
        assert!(!first.tag.contains(" name="));

        // The same name again is a second copy, named for the recipient as
        // the file was; the source changing after does not change a copy.
        fs::write(&source, b"%PDF-2").unwrap();
        let second = attach_file(&draft, Some(&sidecar), &source, 100).unwrap();
        assert_eq!(second.path, sidecar.join("report-2.pdf"));
        assert!(second.tag.contains(" name=\"report.pdf\">"));
        assert_eq!(fs::read(&first.path).unwrap(), b"%PDF-1");

        // What the tags say is what a send reads back.
        let text = format!(
            "{}{}{}",
            fs::read_to_string(&draft).unwrap(),
            first.tag,
            second.tag
        );
        let outgoing = crate::submit::parse_draft(&text).unwrap();
        let parts: Vec<(&Path, &str, &str)> = outgoing
            .parts
            .iter()
            .map(|p| (p.path.as_path(), p.content_type.as_str(), p.name.as_str()))
            .collect();
        assert_eq!(
            parts,
            vec![
                (first.path.as_path(), "application/pdf", "report.pdf"),
                (second.path.as_path(), "application/pdf", "report.pdf"),
            ]
        );
        assert_eq!(outgoing.text, "hi\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_file_leaves_nothing_behind() {
        let dir = scratch("refuse");
        let draft = dir.join("td-mail-draft-1.eml");
        fs::write(&draft, "x").unwrap();
        let sidecar = dir.join("td-mail-att-1");

        // Past the ceiling, not a regular file, or not there: refused
        // before the sidecar is made.
        let big = dir.join("big.bin");
        fs::write(&big, vec![0u8; 11]).unwrap();
        let refused = attach_file(&draft, None, &big, 10).unwrap_err();
        assert!(refused.to_string().contains("past the 10"), "{refused}");
        assert!(attach_file(&draft, None, &dir, 10).is_err());
        assert!(attach_file(&draft, None, &dir.join("gone"), 10).is_err());
        assert!(!sidecar.exists());

        // A sidecar elsewhere, one not private, a draft not so named.
        let elsewhere = scratch("elsewhere");
        let small = dir.join("s.txt");
        fs::write(&small, b"s").unwrap();
        let refused = attach_file(&draft, Some(&elsewhere), &small, 10).unwrap_err();
        assert!(refused.to_string().contains("not beside"), "{refused}");
        fs::DirBuilder::new().mode(0o755).create(&sidecar).unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o755)).unwrap();
        let refused = attach_file(&draft, None, &small, 10).unwrap_err();
        assert!(refused.to_string().contains("private folder"), "{refused}");
        assert_eq!(fs::read_dir(&sidecar).unwrap().count(), 0);
        let odd = dir.join("notes.eml");
        assert!(attach_file(&odd, None, &small, 10).is_err());

        // A name MML cannot carry is cleaned, not refused.
        fs::remove_dir(&sidecar).unwrap();
        let quoted = dir.join("say \"hi\" <now>.txt");
        fs::write(&quoted, b"q").unwrap();
        let attached = attach_file(&draft, None, &quoted, 10).unwrap();
        assert_eq!(attached.name, "say _hi_ _now_.txt");
        assert_eq!(clean_name(" .. "), "attachment");
        assert_eq!(clean_name("a\u{7}b"), "a_b");
        assert_eq!(clean_name("invoice\u{202e}fdp.exe"), "invoice_fdp.exe");
        assert_eq!(clean_name("a\u{2066}b\u{200f}"), "a_b_");

        // A name as long as a component takes has its stem shortened for
        // a second copy, not refused.
        let long = format!("{}.txt", "é".repeat(125));
        assert_eq!(long.len(), 254);
        let (first, _) = claim(&sidecar, &long).unwrap();
        let (second, _) = claim(&sidecar, &long).unwrap();
        assert_eq!(first.file_name().unwrap().len(), 254);
        let second = second.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            second.len() <= NAME_MAX && second.ends_with("-2.txt"),
            "{second}"
        );
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&elsewhere).unwrap();
    }
}
