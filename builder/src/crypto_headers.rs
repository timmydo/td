//! Pinned musl headers for the portable crypto build; no upstream scripts run.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;
type Files = BTreeMap<String, Vec<u8>>;

const ARCHIVE_SIZE: u64 = 1_080_786;
const ARCHIVE_SHA: &str = "a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4";
const HEADER_SHA: &str = "a673b15579a83881c2d9aa61a65ce26dd62557d16c497e5a04e4e6a8bb6d601b";
fn source_receipt() -> String {
    format!(
        "https://musl.libc.org/releases/musl-1.2.5.tar.gz\nsha256 {ARCHIVE_SHA}\n\
         headers x86_64 musl-1.2.5; libc comes from the pinned Rust target std\n"
    )
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(parent: &Path) -> Result<Self> {
        fs::create_dir_all(parent).map_err(|e| format!("headers cache parent: {e}"))?;
        for attempt in 0..128 {
            let path = parent.join(format!("crypto-headers-{}-{attempt}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("headers scratch: {e}")),
            }
        }
        Err("headers scratch names exhausted".into())
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

fn text(path: &Path) -> Result<String> {
    String::from_utf8(read(path, 1024 * 1024)?).map_err(|e| format!("header input UTF-8: {e}"))
}

fn alltypes(input: &str) -> Result<String> {
    let mut output = String::new();
    for line in input.lines() {
        let declaration = if let Some(rest) = line.strip_prefix("TYPEDEF ") {
            let (ty, name) = rest
                .strip_suffix(';')
                .and_then(|rest| rest.rsplit_once(' '))
                .ok_or("invalid musl TYPEDEF")?;
            Some((name.to_string(), format!("typedef {ty} {name};")))
        } else if let Some((kind, rest)) = line
            .strip_prefix("STRUCT ")
            .map(|rest| ("struct", rest))
            .or_else(|| line.strip_prefix("UNION ").map(|rest| ("union", rest)))
        {
            let (name, body) = rest.split_once(' ').ok_or("invalid musl aggregate")?;
            Some((format!("{kind}_{name}"), format!("{kind} {name} {body}")))
        } else {
            None
        };
        if let Some((name, declaration)) = declaration {
            output.push_str(&format!(
                "#if defined(__NEED_{name}) && !defined(__DEFINED_{name})\n\
                 {declaration}\n#define __DEFINED_{name}\n#endif\n\n"
            ));
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    Ok(output)
}

fn syscalls(input: &str) -> String {
    let mut output = input.to_string();
    for line in input.lines().filter(|line| line.contains("__NR_")) {
        output.push_str(&line.replacen("__NR_", "SYS_", 1));
        output.push('\n');
    }
    output
}

fn files(root: &Path, relative: &str, depth: usize, out: &mut Files) -> Result<()> {
    if depth > 3 || out.len() > 512 {
        return Err("header tree exceeds depth or file limit".into());
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
            let entry = entry.map_err(|e| format!("header entry: {e}"))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "header name UTF-8")?;
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
        return Err(format!("non-regular header tree entry: {}", path.display()));
    }
    Ok(())
}

fn header_digest(headers: &Files) -> String {
    let mut hash = crate::sha256::Sha256::new();
    for (name, bytes) in headers {
        hash.update(name.as_bytes());
        hash.update(b"\0");
        hash.update(bytes.len().to_string().as_bytes());
        hash.update(b"\0");
        hash.update(bytes);
    }
    crate::sha256::to_base16(&hash.finalize())
}

fn collect_headers(source: &Path) -> Result<Files> {
    let mut headers = Files::new();
    files(&source.join("include"), "", 0, &mut headers)?;
    headers.retain(|name, _| name.ends_with(".h") && name.split('/').count() <= 2);
    for dir in ["arch/generic/bits", "arch/x86_64/bits"] {
        let mut bits = Files::new();
        files(&source.join(dir), "", 0, &mut bits)?;
        for (name, bytes) in bits {
            if name.ends_with(".h") && !name.contains('/') {
                headers.insert(format!("bits/{name}"), bytes);
            }
        }
    }
    let declarations = text(&source.join("arch/x86_64/bits/alltypes.h.in"))?
        + &text(&source.join("include/alltypes.h.in"))?;
    headers.insert(
        "bits/alltypes.h".into(),
        alltypes(&declarations)?.into_bytes(),
    );
    headers.insert(
        "bits/syscall.h".into(),
        syscalls(&text(&source.join("arch/x86_64/bits/syscall.h.in"))?).into_bytes(),
    );
    Ok(headers)
}

fn generate(source: &Path) -> Result<Files> {
    let headers = collect_headers(source)?;
    if headers.len() != 218 || header_digest(&headers) != HEADER_SHA {
        return Err("musl headers differ from upstream install-headers output".into());
    }
    let mut output: Files = headers
        .into_iter()
        .map(|(name, bytes)| (format!("include/{name}"), bytes))
        .collect();
    output.insert(
        "COPYRIGHT".into(),
        read(&source.join("COPYRIGHT"), 1024 * 1024)?,
    );
    output.insert("SOURCE".into(), source_receipt().into_bytes());
    Ok(output)
}

fn verify_tree(path: &Path, expected: &Files) -> Result<()> {
    let mut actual = Files::new();
    files(path, "", 0, &mut actual)?;
    if &actual != expected {
        return Err(format!("musl header cache differs from verified sources: {}; remove this cache entry and retry", path.display()));
    }
    Ok(())
}

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
    let expected = generate(&source.join("musl-1.2.5"))?;
    publish(&scratch, &parent, &expected)
}

fn publish(scratch: &Scratch, parent: &Path, expected: &Files) -> Result<PathBuf> {
    let destination = parent.join(format!(
        "crypto-musl-x86_64-1.2.5-{}",
        header_digest(expected)
    ));
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_tree(&destination, expected)?;
            return Ok(destination);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat header cache: {e}")),
    }
    let output = scratch.0.join("output");
    for (name, bytes) in expected {
        let path = output.join(name);
        fs::create_dir_all(path.parent().ok_or("header has no parent")?)
            .map_err(|e| format!("create header directory: {e}"))?;
        fs::write(&path, bytes).map_err(|e| format!("write header: {e}"))?;
    }
    verify_tree(&output, expected)?;
    // A concurrent preparer may publish identical bytes first. Never merge.
    if let Err(e) = fs::rename(&output, &destination) {
        verify_tree(&destination, expected)
            .map_err(|why| format!("publish headers: {e}; {why}"))?;
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_preserve_c_layout_and_guard_each_type() {
        assert_eq!(alltypes("TYPEDEF unsigned  long size_t;\n").unwrap(),
            "#if defined(__NEED_size_t) && !defined(__DEFINED_size_t)\ntypedef unsigned  long size_t;\n#define __DEFINED_size_t\n#endif\n\n");
        assert_eq!(alltypes("STRUCT point { int x; };\nUNION value { int x; };\n").unwrap(),
            "#if defined(__NEED_struct_point) && !defined(__DEFINED_struct_point)\nstruct point { int x; };\n#define __DEFINED_struct_point\n#endif\n\n#if defined(__NEED_union_value) && !defined(__DEFINED_union_value)\nunion value { int x; };\n#define __DEFINED_union_value\n#endif\n\n");
        assert!(alltypes("TYPEDEF invalid\n").is_err());
        assert_eq!(
            alltypes("#define value 1\n\n").unwrap(),
            "#define value 1\n\n"
        );
    }

    #[test]
    fn syscall_aliases_preserve_original_and_replace_once() {
        assert_eq!(syscalls("/* header */\n#define __NR_read 0\n#define __NR_alias __NR_read\n"),
            "/* header */\n#define __NR_read 0\n#define __NR_alias __NR_read\n#define SYS_read 0\n#define SYS_alias __NR_read\n");
    }

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
    fn bad_archive_cannot_publish_headers() {
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
    fn collection_overrides_generic_and_excludes_templates_and_deep_files() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        for (path, contents) in [
            ("include/header.h", "public\n"),
            ("include/alltypes.h.in", "TYPEDEF int pid_t;\n"),
            ("include/sys/public.h", "public system\n"),
            ("include/sys/deep/omit.h", "not installed\n"),
            ("arch/generic/bits/override.h", "generic\n"),
            ("arch/generic/bits/retained.h", "retained\n"),
            ("arch/x86_64/bits/override.h", "architecture\n"),
            ("arch/x86_64/bits/alltypes.h.in", "#define _Addr long\n"),
            ("arch/x86_64/bits/syscall.h.in", "#define __NR_read 0\n"),
        ] {
            let path = scratch.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let headers = collect_headers(&scratch.0).unwrap();
        assert_eq!(headers.len(), 6);
        assert_eq!(headers.get("bits/override.h").unwrap(), b"architecture\n");
        assert_eq!(headers.get("bits/retained.h").unwrap(), b"retained\n");
        assert!(headers.contains_key("header.h"));
        assert!(headers.contains_key("sys/public.h"));
        assert!(headers
            .get("bits/alltypes.h")
            .unwrap()
            .starts_with(b"#define _Addr long\n#if defined(__NEED_pid_t)"));
        assert_eq!(
            headers.get("bits/syscall.h").unwrap(),
            b"#define __NR_read 0\n#define SYS_read 0\n"
        );
        assert!(generate(&scratch.0)
            .unwrap_err()
            .contains("upstream install-headers"));
    }

    #[test]
    fn publication_and_reuse_bind_receipt_bytes_to_the_cache_name() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let parent = scratch.0.join("cache");
        fs::create_dir(&parent).unwrap();
        let expected = Files::from([
            ("include/header.h".into(), b"header".to_vec()),
            ("SOURCE".into(), source_receipt().into_bytes()),
        ]);
        let first = publish(&scratch, &parent, &expected).unwrap();
        assert_eq!(publish(&scratch, &parent, &expected).unwrap(), first);
        let mut changed = expected.clone();
        changed.insert("SOURCE".into(), b"new receipt".to_vec());
        let second = publish(&scratch, &parent, &changed).unwrap();
        assert_ne!(first, second);
        verify_tree(&first, &expected).unwrap();
        verify_tree(&second, &changed).unwrap();
        fs::write(first.join("include/header.h"), b"changed").unwrap();
        assert!(publish(&scratch, &parent, &expected)
            .unwrap_err()
            .contains("remove this cache entry and retry"));
        assert!(source_receipt().contains(ARCHIVE_SHA));
    }
}
