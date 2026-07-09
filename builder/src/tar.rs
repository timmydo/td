use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;

pub fn extract_tar(tar: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    let mut file = File::open(tar).map_err(|e| format!("open {}: {e}", tar.display()))?;
    loop {
        let mut header = [0u8; BLOCK];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("read tar header from {}: {e}", tar.display())),
        }
        if header.iter().all(|b| *b == 0) {
            break;
        }

        let entry = Entry::parse(&header)?;
        let out = safe_join(dest, &entry.path)?;
        match entry.kind {
            EntryKind::Directory => {
                fs::create_dir_all(&out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
                skip_entry_data(&mut file, entry.size)
                    .map_err(|e| format!("skip {}: {e}", entry.path.display()))?;
            }
            EntryKind::Regular => {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                }
                let mut out_file =
                    File::create(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
                copy_exact(&mut file, &mut out_file, entry.size)
                    .map_err(|e| format!("extract {}: {e}", entry.path.display()))?;
                fs::set_permissions(&out, fs::Permissions::from_mode(entry.mode))
                    .map_err(|e| format!("chmod {}: {e}", out.display()))?;
            }
            EntryKind::Symlink { target } => {
                if target.is_absolute() || has_parent_component(&target) {
                    return Err(format!(
                        "refusing unsafe tar symlink {} -> {}",
                        entry.path.display(),
                        target.display()
                    ));
                }
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                }
                let _ = fs::remove_file(&out);
                symlink(&target, &out).map_err(|e| {
                    format!("symlink {} -> {}: {e}", out.display(), target.display())
                })?;
                skip_entry_data(&mut file, entry.size)
                    .map_err(|e| format!("skip {}: {e}", entry.path.display()))?;
            }
        }
        skip_padding(&mut file, entry.size)
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
    Symlink { target: PathBuf },
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
            b'5' => EntryKind::Directory,
            b'2' => EntryKind::Symlink {
                target: PathBuf::from(tar_string(field(header, 157, 100)?)?),
            },
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

fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    if rel.is_absolute() || has_parent_component(rel) {
        return Err(format!("refusing unsafe tar path {}", rel.display()));
    }
    Ok(root.join(rel))
}

fn has_parent_component(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
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

fn skip_entry_data(file: &mut File, size: u64) -> io::Result<()> {
    let off = i64::try_from(size).map_err(|_| io::Error::other("tar entry too large to skip"))?;
    file.seek(SeekFrom::Current(off))?;
    Ok(())
}

fn skip_padding(file: &mut File, size: u64) -> io::Result<()> {
    let rem = size % 512;
    if rem == 0 {
        return Ok(());
    }
    let pad = 512 - rem;
    let off = i64::try_from(pad).map_err(|_| io::Error::other("tar padding too large"))?;
    file.seek(SeekFrom::Current(off))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_parent_components() {
        let err = safe_join(Path::new("/tmp/root"), Path::new("../x")).expect_err("unsafe path");
        assert!(err.contains("unsafe tar path"));
    }
}
