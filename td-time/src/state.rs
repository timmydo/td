use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const NOFOLLOW: i32 = 0x20000;
const NONBLOCK: i32 = 0x800;

pub(crate) fn read(path: &Path, limit: usize) -> Result<String, String> {
    let bytes = bytes(path, limit)?;
    String::from_utf8(bytes).map_err(|_| "setting is not UTF-8".into())
}

pub(crate) fn bytes(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err("setting is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("setting grew past its bound".into());
    }
    Ok(bytes)
}

fn directory(path: &Path, owner: u32) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        let meta = fs::symlink_metadata(&current).map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "untrusted time-state directory {}",
                current.display()
            ));
        }
    }
    Ok(())
}

pub(crate) struct State {
    pub(crate) root: PathBuf,
    _lock: File,
}

impl State {
    pub(crate) fn lock() -> Result<Self, String> {
        let root = Path::new("/var/lib/td/time");
        directory(Path::new("/var/lib/td"), 0)?;
        match fs::DirBuilder::new().mode(0o755).create(root) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.to_string()),
        }
        directory(root, 0)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(root.join("lock"))
            .map_err(|e| e.to_string())?;
        let meta = lock.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
            return Err("invalid time-state lock".into());
        }
        lock.try_lock()
            .map_err(|e| format!("time management is busy: {e}"))?;
        Ok(Self {
            root: root.into(),
            _lock: lock,
        })
    }

    pub(crate) fn automatic(&self) -> Result<bool, String> {
        automatic(&self.root)
    }

    pub(crate) fn publish(&self, name: &str, contents: &[u8]) -> Result<(), String> {
        publish(&self.root, name, contents)
    }
}

pub(crate) fn automatic(root: &Path) -> Result<bool, String> {
    let path = root.join("automatic");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.to_string()),
        Ok(meta) if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 => {
            return Err("untrusted automatic-time setting".into());
        }
        Ok(_) => (),
    }
    match read(&path, 4)?.as_str() {
        "on\n" => Ok(true),
        "off\n" => Ok(false),
        _ => Err("invalid automatic-time setting".into()),
    }
}

// The caller holds the common time-state lock and has checked this parent.
pub(crate) fn publish(parent: &Path, name: &str, contents: &[u8]) -> Result<(), String> {
    if !matches!(name, "automatic" | "status" | "timezone" | "localtime") {
        return Err("unsupported time-state file".into());
    }
    let temporary = parent.join(format!(".{name}.td-time-new"));
    match fs::remove_file(&temporary) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(NOFOLLOW)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|e| e.to_string())?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(&temporary, parent.join(name)).map_err(|e| e.to_string())?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}
