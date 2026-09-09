//! Persistent host intent. This record grants no guest or registrar authority.
use crate::{io, vm_git_names, vm_git_profile::Profile, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const LIMIT: u64 = 8704;
const FILE: &str = "workspace";

#[derive(Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub branch: String,
    pub profile: Profile,
}

impl Workspace {
    pub fn new(branch: &str, profile: Profile) -> Result<Self> {
        if !vm_git_names::branch_valid(branch) {
            return Err("invalid task branch; main, HEAD and refs/ names are reserved".into());
        }
        let mut random = [0u8; 16];
        io(
            io(File::open("/dev/urandom"), "open host randomness")?.read_exact(&mut random),
            "read VM workspace identity",
        )?;
        Ok(Self {
            id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            branch: branch.into(),
            profile,
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut fields = text.splitn(4, '\n');
        if text.len() as u64 > LIMIT || fields.next() != Some("TDVM-WORKSPACE-1") {
            return Err("invalid workspace header or size".into());
        }
        let id = fields.next().ok_or("missing workspace identity")?;
        let branch = fields.next().ok_or("missing workspace branch")?;
        if !vm_git_names::instance_valid(id) || !vm_git_names::branch_valid(branch) {
            return Err("invalid workspace identity or branch".into());
        }
        let profile = Profile::parse(fields.next().ok_or("missing workspace profile")?)?;
        Ok(Self {
            id: id.into(),
            branch: branch.into(),
            profile,
        })
    }

    pub fn encode(&self) -> String {
        format!(
            "TDVM-WORKSPACE-1\n{}\n{}\n{}",
            self.id,
            self.branch,
            self.profile.encode()
        )
    }

    pub fn summary(&self) -> Result<String> {
        Ok(format!(
            "Workspace planned: {}\nInstance identity: {}\nOrigin: {}\nGit profile: {}\nGuest key enrollment and cloning pending; branch and starting commit are not reserved.",
            self.branch, self.id, self.profile.repository()?, self.profile.fingerprint()
        ))
    }

    pub fn conflicts(&self, other: &Self) -> Result<bool> {
        Ok(self.id == other.id
            || (self.profile.repository()? == other.profile.repository()?
                && (self.branch == other.branch
                    || self
                        .branch
                        .strip_prefix(&other.branch)
                        .is_some_and(|s| s.starts_with('/'))
                    || other
                        .branch
                        .strip_prefix(&self.branch)
                        .is_some_and(|s| s.starts_with('/')))))
    }

    /// Caller holds the catalog and instance locks; final name is immutable.
    pub fn publish(&self, dir: &Path) -> Result<()> {
        if load(dir)?.is_some() {
            return Err("workspace is already configured".into());
        }
        let temporary = dir.join("workspace.tmp");
        match fs::symlink_metadata(&temporary) {
            Ok(meta) if meta.is_file() => io(
                fs::remove_file(&temporary),
                "remove interrupted workspace staging",
            )?,
            Ok(_) => return Err("unexpected workspace staging entry".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("inspect workspace staging: {e}")),
        }
        let result = (|| {
            crate::write_new(&temporary, self.encode().as_bytes())?;
            io(
                fs::rename(&temporary, dir.join(FILE)),
                "publish VM workspace",
            )?;
            io(
                File::open(dir).and_then(|file| file.sync_all()),
                "sync workspace directory",
            )
        })();
        let _ = fs::remove_file(temporary);
        result
    }
}

pub fn load(dir: &Path) -> Result<Option<Workspace>> {
    let file = match OpenOptions::new()
        .read(true)
        // Linux x86-64 O_NOFOLLOW | O_NONBLOCK; validate before reading.
        .custom_flags(0x20000 | 0x800)
        .open(dir.join(FILE))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("open workspace record: {e}")),
    };
    let meta = io(file.metadata(), "inspect workspace record")?;
    let uid = io(fs::metadata("/proc/self"), "inspect current UID")?.uid();
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
        return Err("workspace record must be a private caller-owned regular file".into());
    }
    let mut text = String::new();
    io(
        file.take(LIMIT + 1).read_to_string(&mut text),
        "read workspace record",
    )?;
    Workspace::parse(&text).map(Some)
}
