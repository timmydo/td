//! Control-plane assembly metadata. Never a target recipe input.
use super::{boot_info, sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new("git")
        .args(["--no-replace-objects", "-C"])
        .arg(root)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("read build source identity: {e}"))?;
    let result = (|| {
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .ok_or("missing Git output")?
            .take(65537)
            .read_to_end(&mut output)
            .map_err(|e| e.to_string())?;
        if output.len() > 65536 {
            return Err("build source identity output exceeds limit".into());
        }
        String::from_utf8(output).map_err(|_| "build source identity is not UTF-8".into())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("read build source identity: {status}"));
    }
    result
}

pub fn capture(root: &Path) -> Result<boot_info::Info, String> {
    let identity = git(root, &["log", "-1", "--format=%H%n%ct", "HEAD"])?;
    let mut rows = identity.lines();
    let commit = rows.next().ok_or("missing source commit")?.to_string();
    let seconds = rows
        .next()
        .ok_or("missing commit timestamp")?
        .parse::<u64>()
        .map_err(|_| "invalid commit timestamp")?;
    if rows.next().is_some() {
        return Err("unexpected source identity".into());
    }
    let dirty = !git(root, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    let info = boot_info::Info {
        commit,
        commit_date: boot_info::utc_date(seconds)?,
        build_date: boot_info::utc_date(now.as_secs())?,
        dirty,
    };
    info.encode()?;
    Ok(info)
}

/// Refuse a build whose source identity changed while it ran. Dirty builds
/// remain explicitly labelled; the committed recipe output is still unchanged.
pub fn finish(root: &Path, before: &boot_info::Info) -> Result<boot_info::Info, String> {
    let after = capture(root)?;
    if before.commit != after.commit || before.dirty != after.dirty {
        return Err(
            "source identity changed during the build; retry from a stable checkout".into(),
        );
    }
    Ok(after)
}

struct DigestWriter(sha256::Sha256);
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn manifest_for(directory: &Path, info: Option<&boot_info::Info>) -> Result<String, String> {
    let mut manifest = String::from("td-deployment-v1\n");
    for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
        let path = directory.join(name);
        if !fs::symlink_metadata(&path)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err(format!(
                "deployment payload is not a regular file: {}",
                path.display()
            ));
        }
        let hash = match info.filter(|_| name == "initramfs.cpio") {
            Some(info) => {
                let mut input = File::open(&path).map_err(|e| e.to_string())?;
                let mut output = DigestWriter(sha256::Sha256::new());
                boot_info::append(&mut input, &mut output, info).map_err(|e| e.to_string())?;
                sha256::to_base16(&output.0.finalize())
            }
            None => sha256::sha256_file(&path).map_err(|e| e.to_string())?,
        };
        manifest.push_str(&format!("{hash}  {name}\n"));
    }
    Ok(manifest)
}

/// Reuse identical assemblies without replacing anything a pending approval
/// can hold. Different signed identities remain distinct retained outputs.
pub fn cached(source: &Path, parent: &Path, info: &boot_info::Info) -> Result<PathBuf, String> {
    if fs::read(source.join("manifest")).map_err(|e| e.to_string())?
        != manifest_for(source, None)?.as_bytes()
    {
        return Err("deployment changed before metadata assembly".into());
    }
    let expected = manifest_for(source, Some(info))?;
    let destination = parent.join(sha256::hex_digest(expected.as_bytes()));
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || fs::read(destination.join("manifest")).map_err(|e| e.to_string())?
                    != expected.as_bytes()
                || manifest_for(&destination, None)? != expected
            {
                return Err("cached deployment assembly does not match its identity".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            assemble(source, &destination, info)?;
            if fs::read(destination.join("manifest")).map_err(|e| e.to_string())?
                != expected.as_bytes()
            {
                return Err("deployment changed during metadata assembly".into());
            }
        }
        Err(error) => return Err(format!("inspect deployment assembly: {error}")),
    }
    Ok(destination)
}

/// Assemble a new unsigned deployment in a caller-owned, absent directory.
/// Check the original manifest before replacing its initramfs digest.
pub fn assemble(source: &Path, destination: &Path, info: &boot_info::Info) -> Result<(), String> {
    let entries = ["bzImage", "initramfs.cpio", "root.erofs"];
    let mut original = String::from("td-deployment-v1\n");
    for name in entries {
        let hash = sha256::sha256_file(&source.join(name)).map_err(|e| e.to_string())?;
        original.push_str(&format!("{hash}  {name}\n"));
    }
    if fs::read(source.join("manifest")).map_err(|e| e.to_string())? != original.as_bytes() {
        return Err("deployment changed before metadata assembly".into());
    }
    fs::create_dir(destination).map_err(|e| format!("create deployment assembly: {e}"))?;
    let result = (|| {
        let mut manifest = String::from("td-deployment-v1\n");
        for name in entries {
            let from = source.join(name);
            let to = destination.join(name);
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&to)
                .map_err(|e| e.to_string())?;
            let mut input = File::open(from).map_err(|e| e.to_string())?;
            if name == "initramfs.cpio" {
                boot_info::append(&mut input, &mut output, info).map_err(|e| e.to_string())?;
            } else {
                std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
            }
            output.sync_all().map_err(|e| e.to_string())?;
            let hash = sha256::sha256_file(&to).map_err(|e| e.to_string())?;
            manifest.push_str(&format!("{hash}  {name}\n"));
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination.join("manifest"))
            .map_err(|e| e.to_string())?;
        file.write_all(manifest.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(destination)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn assembly_rehashes_the_appendix_without_changing_recipe_artifacts() {
        let temp = test_temp::TempDir::new("td-boot-info").unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut manifest = String::from("td-deployment-v1\n");
        for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
            fs::write(source.join(name), name.as_bytes()).unwrap();
            manifest.push_str(&format!(
                "{}  {name}\n",
                sha256::sha256_file(&source.join(name)).unwrap()
            ));
        }
        fs::write(source.join("manifest"), &manifest).unwrap();
        let info = boot_info::Info {
            commit: "a".repeat(40),
            commit_date: "2026-10-24".into(),
            build_date: "2026-10-25".into(),
            dirty: false,
        };
        let destination = temp.path().join("deployment");
        assemble(&source, &destination, &info).unwrap();
        assert_eq!(
            fs::read(source.join("initramfs.cpio")).unwrap(),
            b"initramfs.cpio"
        );
        assert_eq!(
            fs::read_to_string(source.join("manifest")).unwrap(),
            manifest
        );
        let published = fs::read_to_string(destination.join("manifest")).unwrap();
        assert_ne!(published, manifest);
        for name in ["bzImage", "initramfs.cpio", "root.erofs"] {
            assert!(published.contains(&format!(
                "{}  {name}\n",
                sha256::sha256_file(&destination.join(name)).unwrap()
            )));
        }
        assert_eq!(
            boot_info::read(&mut File::open(destination.join("initramfs.cpio")).unwrap()).unwrap(),
            Some(info.clone())
        );
        assert!(assemble(&source, &destination, &info).is_err());
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let first = cached(&source, &cache, &info).unwrap();
        let held = File::open(first.join("manifest")).unwrap();
        assert_eq!(cached(&source, &cache, &info).unwrap(), first);
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            held.metadata().unwrap().ino(),
            fs::metadata(first.join("manifest")).unwrap().ino()
        );
        fs::write(first.join("root.erofs"), b"corrupt cached payload").unwrap();
        assert!(cached(&source, &cache, &info).is_err());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
        fs::write(source.join("bzImage"), b"corrupt").unwrap();
        let refused = temp.path().join("refused");
        assert!(assemble(&source, &refused, &info).is_err());
        assert!(!refused.exists());
        assert_eq!(
            fs::read_to_string(destination.join("manifest")).unwrap(),
            published
        );
    }
}
