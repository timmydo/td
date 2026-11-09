//! The pinned musl 1.2.5 release, kept only as the verified source of the
//! COPYRIGHT notice for the musl libc Rust's self-contained target links into
//! the portable artifacts. Nothing compiles against its headers; no upstream
//! script runs.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;
type Files = BTreeMap<String, Vec<u8>>;

const ARCHIVE_SIZE: u64 = 1_080_786;
const ARCHIVE_SHA: &str = "a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4";

/// The portable targets, both musl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Architecture {
    X86_64,
    Aarch64,
}

impl Architecture {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            _ => Err("portable architecture must be x86_64 or aarch64".into()),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    pub(crate) fn triple(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-linux-musl",
            Self::Aarch64 => "aarch64-unknown-linux-musl",
        }
    }
}

fn source_receipt() -> String {
    format!(
        "https://musl.libc.org/releases/musl-1.2.5.tar.gz\nsha256 {ARCHIVE_SHA}\n\
         notice musl-1.2.5 COPYRIGHT; libc comes from the pinned Rust target std\n"
    )
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(parent: &Path) -> Result<Self> {
        fs::create_dir_all(parent).map_err(|e| format!("musl notice cache parent: {e}"))?;
        for attempt in 0..128 {
            let path = parent.join(format!(
                "crypto-musl-notice-{}-{attempt}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("musl notice scratch: {e}")),
            }
        }
        Err("musl notice scratch names exhausted".into())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if !fs::symlink_metadata(path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .file_type()
        .is_file()
    {
        return Err(format!("not a regular file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("file exceeds input limit: {}", path.display()));
    }
    Ok(bytes)
}

fn files(root: &Path, relative: &str, depth: usize, out: &mut Files) -> Result<()> {
    if depth > 3 || out.len() > 512 {
        return Err("musl notice tree exceeds depth or file limit".into());
    }
    // Joining an empty suffix adds '/', which makes lstat follow a root link.
    let path = if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let kind = fs::symlink_metadata(&path)
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .file_type();
    if kind.is_dir() {
        for entry in fs::read_dir(&path).map_err(|e| format!("read directory: {e}"))? {
            let entry = entry.map_err(|e| format!("musl notice entry: {e}"))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "musl notice name UTF-8")?;
            let next = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            files(root, &next, depth + 1, out)?;
        }
    } else if kind.is_file() {
        out.insert(relative.to_string(), read(&path, 1024 * 1024)?);
    } else {
        return Err(format!("non-regular musl notice entry: {}", path.display()));
    }
    Ok(())
}

fn tree_digest(tree: &Files) -> String {
    let mut hash = crate::sha256::Sha256::new();
    for (name, bytes) in tree {
        hash.update(name.as_bytes());
        hash.update(b"\0");
        hash.update(bytes.len().to_string().as_bytes());
        hash.update(b"\0");
        hash.update(bytes);
    }
    crate::sha256::to_base16(&hash.finalize())
}

fn verify_tree(path: &Path, expected: &Files) -> Result<()> {
    let mut actual = Files::new();
    files(path, "", 0, &mut actual)?;
    if &actual != expected {
        return Err(format!("musl notice cache differs from verified sources: {}; remove this cache entry and retry", path.display()));
    }
    Ok(())
}

/// Verify the archive, then publish its COPYRIGHT with a source receipt.
pub(crate) fn prepare(root: &Path, archive: &Path) -> Result<PathBuf> {
    // Hash the bytes we extract, not a path reopened after verification.
    let bytes = read(archive, ARCHIVE_SIZE)?;
    if bytes.len() as u64 != ARCHIVE_SIZE || crate::sha256::hex_digest(&bytes) != ARCHIVE_SHA {
        return Err("musl-1.2.5 archive size or SHA-256 mismatch".into());
    }
    let parent = root.join(".td-build-cache");
    let scratch = Scratch::new(&parent)?;
    let source = scratch.0.join("source");
    let tar = crate::gzip::decompress_bytes(&bytes)?;
    crate::tar::extract_tar_reader(&mut Cursor::new(tar), "verified musl-1.2.5", &source)?;
    let expected = Files::from([
        (
            "COPYRIGHT".to_string(),
            read(&source.join("musl-1.2.5/COPYRIGHT"), 1024 * 1024)?,
        ),
        ("SOURCE".to_string(), source_receipt().into_bytes()),
    ]);
    publish(&scratch, &parent, &expected)
}

fn publish(scratch: &Scratch, parent: &Path, expected: &Files) -> Result<PathBuf> {
    let destination = parent.join(format!("crypto-musl-1.2.5-{}", tree_digest(expected)));
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_tree(&destination, expected)?;
            return Ok(destination);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat musl notice cache: {e}")),
    }
    let output = scratch.0.join("output");
    for (name, bytes) in expected {
        let path = output.join(name);
        fs::create_dir_all(path.parent().ok_or("musl notice has no parent")?)
            .map_err(|e| format!("create musl notice directory: {e}"))?;
        fs::write(&path, bytes).map_err(|e| format!("write musl notice: {e}"))?;
    }
    verify_tree(&output, expected)?;
    // A concurrent preparer may publish identical bytes first. Never merge.
    if let Err(e) = fs::rename(&output, &destination) {
        verify_tree(&destination, expected)
            .map_err(|why| format!("publish musl notice: {e}; {why}"))?;
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_rejects_modified_added_missing_and_symlink_files() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let path = scratch.0.join("cache");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("header.h"), b"original").unwrap();
        let expected = Files::from([("header.h".into(), b"original".to_vec())]);
        verify_tree(&path, &expected).unwrap();
        let link = scratch.0.join("linked-root");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(verify_tree(&link, &expected).is_err());
        fs::write(path.join("header.h"), b"modified").unwrap();
        assert!(verify_tree(&path, &expected).is_err());
        fs::remove_file(path.join("header.h")).unwrap();
        assert!(verify_tree(&path, &expected).is_err());
        std::os::unix::fs::symlink("../outside", path.join("header.h")).unwrap();
        assert!(verify_tree(&path, &expected).is_err());
        fs::remove_file(path.join("header.h")).unwrap();
        fs::write(path.join("header.h"), b"original").unwrap();
        fs::write(path.join("extra.h"), b"extra").unwrap();
        assert!(verify_tree(&path, &expected).is_err());
    }

    #[test]
    fn bad_archive_cannot_publish_a_notice() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let archive = scratch.0.join("source.tar.gz");
        fs::write(&archive, b"not the pinned musl archive").unwrap();
        assert!(prepare(&scratch.0, &archive)
            .unwrap_err()
            .contains("SHA-256 mismatch"));
        fs::write(&archive, vec![0; ARCHIVE_SIZE as usize]).unwrap();
        assert!(prepare(&scratch.0, &archive)
            .unwrap_err()
            .contains("SHA-256 mismatch"));
        assert!(!scratch.0.join(".td-build-cache").exists());
    }

    #[test]
    fn architecture_selection_cannot_fall_back() {
        assert_eq!(Architecture::parse("x86_64").unwrap(), Architecture::X86_64);
        assert_eq!(
            Architecture::parse("aarch64").unwrap(),
            Architecture::Aarch64
        );
        for invalid in ["", "arm64", "../x86_64", "aarch64-unknown-linux-musl"] {
            assert!(Architecture::parse(invalid).is_err());
        }
        assert_eq!(Architecture::Aarch64.triple(), "aarch64-unknown-linux-musl");
    }

    #[test]
    fn publication_and_reuse_bind_receipt_bytes_to_the_cache_name() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let parent = scratch.0.join("cache");
        fs::create_dir(&parent).unwrap();
        let expected = Files::from([
            ("COPYRIGHT".into(), b"notice".to_vec()),
            ("SOURCE".into(), source_receipt().into_bytes()),
        ]);
        let first = publish(&scratch, &parent, &expected).unwrap();
        assert!(first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("crypto-musl-1.2.5-"));
        assert_eq!(publish(&scratch, &parent, &expected).unwrap(), first);
        let mut changed = expected.clone();
        changed.insert("SOURCE".into(), b"new receipt".to_vec());
        let second = publish(&scratch, &parent, &changed).unwrap();
        assert_ne!(first, second);
        verify_tree(&first, &expected).unwrap();
        verify_tree(&second, &changed).unwrap();
        fs::write(first.join("COPYRIGHT"), b"changed").unwrap();
        assert!(publish(&scratch, &parent, &expected)
            .unwrap_err()
            .contains("remove this cache entry and retry"));
        assert!(source_receipt().contains(ARCHIVE_SHA));
    }
}
