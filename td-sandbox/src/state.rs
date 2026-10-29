use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const MAX_SHARES: usize = 30;
pub struct State {
    pub root: PathBuf,
    pub home: PathBuf,
    pub network: bool,
    pub shares: Vec<(PathBuf, bool)>,
    _lock: File,
}

pub fn name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(io::Error::other(
            "sandbox name must be 1–32 lowercase letters, digits or hyphens",
        ));
    }
    Ok(())
}

pub fn base() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unset"))?;
    let root = PathBuf::from(home).join(".local/share/td-sandbox");
    td_fs::private_dir(&root)?;
    fs::canonicalize(root)
}

pub fn text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .filter(|p| !p.contains(['\n', '\r', '\0']))
        .ok_or_else(|| io::Error::other("path must be UTF-8 without line breaks"))
}

fn open_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(0o400_000 | 0o4000)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != fs::metadata("/proc/self")?.uid()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(io::Error::other(
            "sandbox metadata is not a private, singly linked regular file",
        ));
    }
    Ok(file)
}

impl State {
    pub fn create(base: &Path, name_: &str) -> io::Result<()> {
        Self::create_initial(base, name_, "format=1\nnetwork=off\n", None)
    }

    fn create_initial(
        base: &Path,
        name_: &str,
        policy: &str,
        shared: Option<&Path>,
    ) -> io::Result<()> {
        name(name_)?;
        let root = base.join(name_);
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let result = (|| {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join("home"))?;
            if let Some(shared) = shared {
                std::os::unix::fs::symlink(shared, root.join("home/sandbox"))?;
            }
            let mut file = open_private(&root.join("policy"))?;
            file.write_all(policy.as_bytes())?;
            file.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&root);
        }
        result
    }

    /// Seed only a missing profile; existing policy belongs to the user.
    pub fn init_default(base: &Path, account_home: &Path) -> io::Result<()> {
        match fs::symlink_metadata(base.join("default")) {
            Ok(_) => return Self::open(base, "default").map(|_| ()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let shared = account_home.join("sandbox");
        td_fs::private_dir(&shared)?;
        let shared = fs::canonicalize(shared)?;
        if shared.starts_with(base) || base.starts_with(&shared) {
            return Err(io::Error::other("default share overlaps sandbox storage"));
        }
        let policy = format!("format=1\nnetwork=off\nwrite={}\n", text(&shared)?);
        Self::create_initial(base, "default", &policy, Some(&shared))
    }

    pub fn open(base: &Path, name_: &str) -> io::Result<Self> {
        name(name_)?;
        let root = base.join(name_);
        td_fs::check_private_dir(&root)?;
        td_fs::check_private_dir(&root.join("home"))?;
        let lock = open_private(&root.join("lock"))?;
        lock.try_lock()
            .map_err(|e| io::Error::other(format!("sandbox is active or being changed: {e}")))?;
        let policy = td_fs::read_bounded_real_file(&root.join("policy"), "sandbox policy", 65536)?;
        let policy = String::from_utf8(policy).map_err(io::Error::other)?;
        let (network, shares) = parse(&policy)?;
        Ok(Self {
            home: root.join("home"),
            root,
            network,
            shares,
            _lock: lock,
        })
    }

    pub fn save(&self) -> io::Result<()> {
        let mut policy = format!(
            "format=1\nnetwork={}\n",
            if self.network { "host" } else { "off" }
        );
        for (path, write) in &self.shares {
            policy.push_str(&format!(
                "{}={}\n",
                if *write { "write" } else { "read" },
                text(path)?
            ));
        }
        td_fs::replace(&self.root.join("policy"), policy.as_bytes(), 0o600)
    }

    pub fn share(&mut self, base: &Path, path: &Path, write: bool) -> io::Result<()> {
        let path = fs::canonicalize(path)?;
        text(&path)?;
        if !path.is_dir() || path.starts_with(base) || base.starts_with(&path) {
            return Err(io::Error::other(
                "share must be a directory outside sandbox management storage",
            ));
        }
        if self
            .shares
            .iter()
            .any(|(other, _)| path.starts_with(other) || other.starts_with(&path))
            || self.shares.len() >= MAX_SHARES
        {
            return Err(io::Error::other(
                "shares overlap or exceed the 30-directory limit",
            ));
        }
        self.shares.push((path, write));
        self.save()
    }
}

fn parse(policy: &str) -> io::Result<(bool, Vec<(PathBuf, bool)>)> {
    let mut lines = policy.lines();
    if lines.next() != Some("format=1") {
        return Err(io::Error::other("unsupported sandbox policy"));
    }
    let network = match lines.next() {
        Some("network=host") => true,
        Some("network=off") => false,
        _ => return Err(io::Error::other("invalid network policy")),
    };
    let mut shares = Vec::new();
    for line in lines {
        let (key, path) = line
            .split_once('=')
            .ok_or_else(|| io::Error::other("invalid share"))?;
        if !matches!(key, "read" | "write")
            || !Path::new(path).is_absolute()
            || shares.len() >= MAX_SHARES
        {
            return Err(io::Error::other("invalid share"));
        }
        text(Path::new(path))?;
        shares.push((PathBuf::from(path), key == "write"));
    }
    Ok((network, shares))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_seeds_the_share_once_and_preserves_user_changes() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-default")?;
        let base = home.join(".local/share/td-sandbox");
        td_fs::private_dir(&base)?;
        State::init_default(&base, &home)?;
        let shared = fs::canonicalize(home.join("sandbox"))?;
        td_fs::check_private_dir(&shared)?;
        let mut state = State::open(&base, "default")?;
        assert!(!state.network);
        assert_eq!(state.shares, [(shared.clone(), true)]);
        assert_eq!(fs::read_link(state.home.join("sandbox"))?, shared);
        fs::write(state.home.join("sandbox/from-private-home"), "shared")?;
        assert_eq!(
            fs::read_to_string(home.join("sandbox/from-private-home"))?,
            "shared"
        );
        state.network = true;
        state.shares.clear();
        state.save()?;
        fs::write(state.home.join("keep"), "private")?;
        drop(state);
        State::init_default(&base, &home)?;
        let state = State::open(&base, "default")?;
        assert!(state.network);
        assert!(state.shares.is_empty());
        assert_eq!(fs::read_to_string(state.home.join("keep"))?, "private");
        Ok(())
    }

    #[test]
    fn default_refuses_links_and_incomplete_existing_profiles() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-default-invalid")?;
        let other = td_fs::TempDir::new("sandbox-default-outside")?;
        let base = home.join("state");
        td_fs::private_dir(&base)?;
        std::os::unix::fs::symlink(other.path(), home.join("sandbox"))?;
        assert!(State::init_default(&base, &home).is_err());
        assert!(!base.join("default").exists());
        fs::remove_file(home.join("sandbox"))?;
        std::os::unix::fs::symlink(other.path(), base.join("default"))?;
        assert!(State::init_default(&base, &home).is_err());
        fs::remove_file(base.join("default"))?;
        td_fs::private_dir(&base.join("default"))?;
        assert!(State::init_default(&base, &home).is_err());
        assert!(!base.join("default/policy").exists());
        Ok(())
    }

    #[test]
    fn state_is_private_and_serialized() -> io::Result<()> {
        let base = td_fs::TempDir::new("sandbox-state")?;
        State::create(&base, "tools")?;
        let mut state = State::open(&base, "tools")?;
        assert!(State::open(&base, "tools").is_err());
        assert!(state.share(&base, &base, true).is_err());
        state.network = true;
        state.save()?;
        drop(state);
        assert!(State::open(&base, "tools")?.network);
        assert!(name("../escape").is_err());
        assert!(parse("format=1\nnetwork=host\nentry=/bin/sh\n").is_err());
        Ok(())
    }
}
