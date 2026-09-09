//! Persistent workspace intent and recoverable Git enrollment lifecycle.
use crate::{io, vm_git_names, vm_git_profile::Profile, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const LIMIT: u64 = 8960;
const FILE: &str = "workspace";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pending,
    Enrolled,
    Revoking,
}
impl Phase {
    fn text(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Enrolled => "enrolled",
            Self::Revoking => "revoking",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "enrolled" => Ok(Self::Enrolled),
            "revoking" => Ok(Self::Revoking),
            _ => Err("invalid workspace enrollment phase".into()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Enrollment {
    pub phase: Phase,
    pub key: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub branch: String,
    pub profile: Profile,
    pub enrollment: Option<Enrollment>,
    pub start: Option<String>,
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
            enrollment: None,
            start: None,
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.len() as u64 > LIMIT {
            return Err("workspace exceeds size limit".into());
        }
        let (header, rest) = text.split_once('\n').ok_or("missing workspace header")?;
        if !matches!(header, "TDVM-WORKSPACE-1" | "TDVM-WORKSPACE-2" | "TDVM-WORKSPACE-3") {
            return Err("invalid workspace header".into());
        }
        let count = match header { "TDVM-WORKSPACE-1" => 3, "TDVM-WORKSPACE-2" => 5, _ => 6 };
        let mut fields = rest.splitn(count, '\n');

        let id = fields.next().ok_or("missing workspace identity")?;
        let branch = fields.next().ok_or("missing workspace branch")?;
        if !vm_git_names::instance_valid(id) || !vm_git_names::branch_valid(branch) {
            return Err("invalid workspace identity or branch".into());
        }
        let enrollment = if header != "TDVM-WORKSPACE-1" {
            let phase = Phase::parse(fields.next().ok_or("missing enrollment phase")?)?;
            let key = fields.next().ok_or("missing enrolled public key")?;
            crate::vm_wire::git_key::key(key)?;
            Some(Enrollment {
                phase,
                key: key.into(),
            })
        } else {
            None
        };
        let start = if header == "TDVM-WORKSPACE-3" {
            if enrollment.as_ref().is_none_or(|state| state.phase == Phase::Pending) {
                return Err("starting commit requires an acknowledged enrollment".into());
            }
            Some(fields.next().ok_or("missing starting commit")?.to_string())
        } else { None };
        let profile = Profile::parse(fields.next().ok_or("missing workspace profile")?)?;
        if let Some(oid) = &start {
            crate::vm_git_origin::Origin::new(profile.repository()?.into(), oid.clone())?;
        }
        Ok(Self {
            id: id.into(),
            branch: branch.into(),
            profile,
            enrollment,
            start,
        })
    }

    pub fn encode(&self) -> String {
        match &self.enrollment {
            None => format!(
                "TDVM-WORKSPACE-1\n{}\n{}\n{}",
                self.id,
                self.branch,
                self.profile.encode()
            ),
            Some(state) => format!(
                "{}\n{}\n{}\n{}\n{}\n{}{}",
                if self.start.is_some() { "TDVM-WORKSPACE-3" } else { "TDVM-WORKSPACE-2" },
                self.id,
                self.branch,
                state.phase.text(),
                state.key,
                self.start.as_ref().map_or_else(String::new, |oid| format!("{oid}\n")),
                self.profile.encode()
            ),
        }
    }

    pub fn summary(&self) -> Result<String> {
        let state = match self.enrollment.as_ref().map(|value| value.phase) {
            None => "Guest key enrollment and cloning pending; branch and starting commit are not reserved.",
            Some(Phase::Pending) => "Git key enrollment outcome unconfirmed; retry enrollment. Cloning pending.",
            Some(Phase::Enrolled) => "Git key enrollment and task-branch reservation recorded. Use workspace clone to prepare or inspect the guest clone.",
            Some(Phase::Revoking) => "Git key revocation pending; retry deletion after stopping the VM. Enrollment is disabled.",
        };
        Ok(format!(
            "Workspace planned: {}\nInstance identity: {}\nOrigin: {}\nGit profile: {}\nStarting commit: {}\n{state}",
            self.branch,
            self.id,
            self.profile.repository()?,
            self.profile.fingerprint(),
            self.start.as_deref().unwrap_or("not retained")
        ))
    }

    /// The instance lock spans the transition and any external operation.
    pub fn transition(&mut self, dir: &Path, phase: Phase, key: &str) -> Result<()> {
        crate::vm_wire::git_key::key(key)?;
        let current = load(dir)?.ok_or("workspace disappeared")?;
        if current != *self {
            return Err("workspace changed before enrollment transition".into());
        }
        let allowed = match &self.enrollment {
            None => phase == Phase::Pending,
            Some(state) => {
                state.key == key
                    && match state.phase {
                        Phase::Pending => true,
                        Phase::Enrolled => matches!(phase, Phase::Enrolled | Phase::Revoking),
                        Phase::Revoking => phase == Phase::Revoking,
                    }
            }
        };
        if !allowed {
            return Err("refusing workspace key replacement or enrollment reversal".into());
        }
        let next = Self {
            id: self.id.clone(),
            branch: self.branch.clone(),
            profile: self.profile.clone(),
            enrollment: Some(Enrollment {
                phase,
                key: key.into(),
            }),
            start: self.start.clone(),
        };
        next.save(dir)?;
        *self = next;
        Ok(())
    }

    pub fn record_start(&mut self, dir: &Path, oid: &str) -> Result<()> {
        crate::vm_git_origin::Origin::new(self.profile.repository()?.into(), oid.into())?;
        if load(dir)?.as_ref() != Some(self)
            || self.enrollment.as_ref().is_none_or(|state| state.phase != Phase::Enrolled)
            || self.start.as_ref().is_some_and(|old| old != oid)
        {
            return Err("refusing changed workspace or starting-commit replacement".into());
        }
        let next = Self {
            id: self.id.clone(), branch: self.branch.clone(), profile: self.profile.clone(),
            enrollment: self.enrollment.as_ref().map(|state| Enrollment { phase: state.phase, key: state.key.clone() }),
            start: Some(oid.into()),
        };
        next.save(dir)?;
        *self = next;
        Ok(())
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
        self.save(dir)
    }

    fn save(&self, dir: &Path) -> Result<()> {
        if Self::parse(&self.encode())? != *self {
            return Err("workspace does not round trip".into());
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
            )?;
            for parent in dir.ancestors().skip(1) {
                io(File::open(parent).and_then(|file| file.sync_all()), "sync workspace publication ancestor")?;
            }
            Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recorded_start_is_immutable_and_survives_revocation() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("td-start-state-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos()));
        fs::create_dir(&dir).map_err(|e| e.to_string())?;
        let result = (|| {
            let profile = Profile::parse(&format!("TDVM-GIT-PROFILE-1\nrepository=/srv/git/td.git\naddress=10.0.2.2\nport=22\nuser=test\nserver-uid=1001\nsocket=/home/test/.td-vm-registrar\nregistrar=/usr/local/libexec/td-vm-registrar\ngit=/bin/git\nhost-key={KEY}\nauthor-name=Fixture\nauthor-email=fixture@example.invalid\n"))?;
            let mut workspace = Workspace::new("task", profile)?;
            workspace.publish(&dir)?;
            let oid = "a".repeat(40);
            assert!(workspace.record_start(&dir, &oid).is_err());
            workspace.transition(&dir, Phase::Pending, KEY)?;
            assert!(workspace.record_start(&dir, &oid).is_err());
            workspace.transition(&dir, Phase::Enrolled, KEY)?;
            workspace.record_start(&dir, &oid)?;
            let encoded = workspace.encode();
            assert!(encoded.starts_with("TDVM-WORKSPACE-3\n"));
            assert_eq!(load(&dir)?.as_ref(), Some(&workspace));
            workspace.record_start(&dir, &oid)?;
            assert!(workspace.record_start(&dir, &"b".repeat(40)).is_err());
            assert_eq!(load(&dir)?.ok_or("missing workspace")?.encode(), encoded);
            assert!(Workspace::parse(&encoded.replace(&oid, &"0".repeat(40))).is_err());
            assert!(Workspace::parse(&encoded.replace("\nenrolled\n", "\npending\n")).is_err());
            workspace.transition(&dir, Phase::Revoking, KEY)?;
            assert_eq!(load(&dir)?.ok_or("missing workspace")?.start.as_deref(), Some(oid.as_str()));
            assert!(workspace.record_start(&dir, &oid).is_err());
            assert!(workspace.transition(&dir, Phase::Enrolled, KEY).is_err());
            Ok(())
        })();
        let _ = fs::remove_dir_all(dir);
        result
    }
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
}
