//! Root preparation of the portal's fixed read-only Downloads view.

use crate::{launch, mount_sys};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const HUMAN: u32 = 1000;
const PORTAL: u32 = 991;
const VIEW: &str = "/var/td-portal-files/1000/Downloads";
const DEADLINE: Duration = Duration::from_secs(5);
const MAX_MOUNTINFO: u64 = 1024 * 1024;
const DIRECTORY_FLAGS: i32 = 0x10000 | 0x20000;

fn pinned(parent: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/{}", parent.as_raw_fd(), name))
}

fn directory(path: &Path, owner: u32, trusted: bool) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY_FLAGS)
        .open(path)
        .map_err(|e| format!("open portal file-grant directory: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != owner
        || metadata.gid() != owner
        || (trusted && metadata.mode() & 0o022 != 0)
    {
        return Err(format!(
            "portal file-grant directory {} has uid/gid {}/{} mode {:04o}; expected {owner}/{owner}{}",
            path.display(), metadata.uid(), metadata.gid(), metadata.mode() & 0o7777,
            if trusted { " without other writers" } else { "" },
        ));
    }
    Ok(file)
}

fn child(parent: &File, name: &str, owner: u32, trusted: bool) -> Result<File, String> {
    directory(&pinned(parent, name), owner, trusted)
}

fn ensure_root_child(parent: &File, name: &str) -> Result<File, String> {
    let created = match fs::DirBuilder::new()
        .mode(0o755)
        .create(pinned(parent, name))
    {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(format!("create portal file-grant directory: {e}")),
    };
    let directory = child(parent, name, 0, true)?;
    if created {
        directory
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    if directory.metadata().map_err(|e| e.to_string())?.mode() & 0o7777 != 0o755 {
        return Err("portal file-grant parent must have mode 0755".into());
    }
    Ok(directory)
}

fn mount_options() -> Result<Option<String>, String> {
    let mut text = String::new();
    File::open("/proc/self/mountinfo")
        .map_err(|e| e.to_string())?
        .take(MAX_MOUNTINFO + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_MOUNTINFO {
        return Err("mount table exceeds portal file-grant bound".into());
    }
    mount_options_from(&text)
}

fn mount_options_from(text: &str) -> Result<Option<String>, String> {
    let mut result = None;
    for line in text.lines() {
        let Some((left, _)) = line.split_once(" - ") else {
            return Err("malformed portal file-grant mount table".into());
        };
        let mut fields = left.split_ascii_whitespace();
        if fields.nth(4) == Some(VIEW) {
            if result.is_some() {
                return Err("portal file-grant has stacked mounts".into());
            }
            result = Some(fields.next().ok_or("missing mount options")?.to_string());
        }
    }
    Ok(result)
}

fn require_view(source: &File, parent: &File, options: &str) -> Result<(), String> {
    for required in ["ro", "nosuid", "nodev", "noexec"] {
        if !options.split(',').any(|option| option == required) {
            return Err(format!("portal file-grant mount lacks {required}"));
        }
    }
    let source = source.metadata().map_err(|e| e.to_string())?;
    let view = child(parent, "Downloads", PORTAL, false)?;
    let view = view.metadata().map_err(|e| e.to_string())?;
    if source.dev() != view.dev() || source.ino() != view.ino() {
        return Err("portal file-grant no longer names the configured Downloads directory".into());
    }
    Ok(())
}

struct NamespaceChild(Child);

impl Drop for NamespaceChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn namespace() -> Result<File, String> {
    let (mut parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
    parent
        .set_read_timeout(Some(DEADLINE))
        .map_err(|e| e.to_string())?;
    parent
        .set_write_timeout(Some(DEADLINE))
        .map_err(|e| e.to_string())?;
    let mut child = NamespaceChild(
        Command::new("/bin/td-authd")
            .arg("portal-file-namespace")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start portal mapping helper: {e}"))?,
    );
    let mut ready = [0];
    parent
        .read_exact(&mut ready)
        .map_err(|e| format!("portal mapping helper: {e}"))?;
    if ready != [1] {
        return Err("invalid portal mapping helper greeting".into());
    }
    // The unreaped direct child cannot have its PID reassigned. It waits on
    // this private endpoint and never delegates it or executes another image.
    let process = PathBuf::from(format!("/proc/{}", child.0.id()));
    let mapping = format!("{HUMAN} {PORTAL} 1\n");
    fs::write(process.join("uid_map"), &mapping).map_err(|e| e.to_string())?;
    fs::write(process.join("setgroups"), "deny\n").map_err(|e| e.to_string())?;
    fs::write(process.join("gid_map"), &mapping).map_err(|e| e.to_string())?;
    let namespace = File::open(process.join("ns/user")).map_err(|e| e.to_string())?;
    parent.write_all(&[2]).map_err(|e| e.to_string())?;
    parent.read_exact(&mut ready).map_err(|e| e.to_string())?;
    if ready != [3] || !child.0.wait().map_err(|e| e.to_string())?.success() {
        return Err("portal mapping helper did not finish successfully".into());
    }
    Ok(namespace)
}

pub(crate) fn namespace_helper() -> Result<(), String> {
    launch::require_launch_startup()?;
    let endpoint = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| e.to_string())?;
    let mut endpoint = UnixStream::from(endpoint);
    endpoint
        .set_read_timeout(Some(DEADLINE))
        .map_err(|e| e.to_string())?;
    endpoint
        .set_write_timeout(Some(DEADLINE))
        .map_err(|e| e.to_string())?;
    mount_sys::new_user_namespace().map_err(|e| format!("create portal mapping namespace: {e}"))?;
    endpoint.write_all(&[1]).map_err(|e| e.to_string())?;
    let mut finish = [0];
    endpoint
        .read_exact(&mut finish)
        .map_err(|e| e.to_string())?;
    if finish != [2] {
        return Err("invalid portal mapping helper completion".into());
    }
    endpoint.write_all(&[3]).map_err(|e| e.to_string())
}

pub(crate) fn prepare() -> Result<(), String> {
    launch::require_launch_startup()?;
    let root = directory(Path::new("/"), 0, true)?;
    let var = child(&root, "var", 0, true)?;
    let home = child(&var, "home", 0, true)?;
    let human = child(&home, "tester", HUMAN, true)?;
    let source = child(&human, "Downloads", HUMAN, false)?;
    let grants = ensure_root_child(&var, "td-portal-files")?;
    let session = ensure_root_child(&grants, "1000")?;
    if let Some(options) = mount_options()? {
        return require_view(&source, &session, &options);
    }
    let target = ensure_root_child(&session, "Downloads")?;
    let namespace = namespace()?;
    let mount = mount_sys::clone_directory(source.as_fd()).map_err(|e| e.to_string())?;
    mount_sys::portal_attributes(mount.as_fd(), namespace.as_fd()).map_err(|e| e.to_string())?;
    mount_sys::publish(mount.as_fd(), target.as_fd()).map_err(|e| e.to_string())?;
    let options = mount_options()?.ok_or("published portal file-grant mount is missing")?;
    require_view(&source, &session, &options)
}

/// The supervisor calls this after all services stop and before unmounting /var.
pub(crate) fn release() -> Result<(), String> {
    launch::require_launch_startup()?;
    if mount_options()?.is_none() {
        return Ok(());
    }
    let status = Command::new("/bin/umount")
        .arg(VIEW)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("start fixed portal unmount: {e}"))?;
    if !status.success() {
        return Err(format!("fixed portal unmount failed: {status}"));
    }
    if mount_options()?.is_some() {
        return Err("portal file-grant mount remains after unmount".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/portal_files.rs"]
mod tests;
