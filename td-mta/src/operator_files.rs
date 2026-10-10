//! Protected operator-input opening under STORAGE.md's trusted stable-path
//! contract. Checks are not atomic against a hostile namespace writer, and std
//! does not verify the process UID.
use crate::{
    config::{inputs, values},
    store_fs::PrivateRoot,
};
use std::{
    fmt,
    fs::{self, File, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::Path,
};

/// The main configuration plus every inventory slot.
pub const MAX_INPUTS: usize = inputs::MAX_SLOTS + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// The main operator file; non-secret, opened before the owner is bound.
    Configuration,
    Public,
    /// Relay passwords and private keys: 0400 or 0600 only.
    Secret,
}
impl Role {
    pub const fn of(target: inputs::Target) -> Self {
        if target.requires_private_mode() {
            Self::Secret
        } else {
            Self::Public
        }
    }
}

/// Fixed codes; no variant carries a path or file bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Path,
    Unbound,
    Type,
    Owner,
    Writable,
    Mode,
    PrivateMode,
    Changed,
    Aliased,
    Capacity,
    Io(io::ErrorKind),
}
impl Error {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Path => "input-path",
            Self::Unbound => "input-owner-unbound",
            Self::Type => "input-type",
            Self::Owner => "input-owner",
            Self::Writable => "input-writable",
            Self::Mode => "input-mode",
            Self::PrivateMode => "input-private-mode",
            Self::Changed => "input-changed",
            Self::Aliased => "input-secret-alias",
            Self::Capacity => "input-capacity",
            Self::Io(io::ErrorKind::NotFound) => "input-not-found",
            Self::Io(io::ErrorKind::PermissionDenied) => "input-denied",
            Self::Io(_) => "input-io",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

#[derive(Clone, Copy)]
enum Owner {
    /// Before binding, every non-root owner must be one UID the root later has.
    Unbound(Option<u32>),
    Bound(u32),
}
impl Owner {
    fn admit(&mut self, uid: u32) -> Result<(), Error> {
        match self {
            _ if uid == 0 => Ok(()),
            Self::Bound(owner) | Self::Unbound(Some(owner)) if *owner == uid => Ok(()),
            Self::Unbound(seen @ None) => {
                *seen = Some(uid);
                Ok(())
            }
            _ => Err(Error::Owner),
        }
    }
}

#[derive(Clone, Copy)]
struct Seen {
    device: u64,
    inode: u64,
    secret: bool,
}

/// One configuration load's opened inputs. Device/inode identities keep a
/// secret file from also serving as any non-secret input, including through
/// hard links. This is request-local evidence, not runtime publication.
pub struct Inputs {
    owner: Owner,
    seen: [Seen; MAX_INPUTS],
    count: usize,
}
impl fmt::Debug for Inputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OperatorInputs(<redacted>)")
    }
}
impl Default for Inputs {
    fn default() -> Self {
        Self::new()
    }
}
impl Inputs {
    pub const fn new() -> Self {
        Self {
            owner: Owner::Unbound(None),
            seen: [Seen {
                device: 0,
                inode: 0,
                secret: false,
            }; MAX_INPUTS],
            count: 0,
        }
    }
    /// Distinct opened file identities.
    pub fn count(&self) -> usize {
        self.count
    }
    /// The expected UID comes from the checked private data root, never an
    /// input. Owners already admitted for the main configuration must agree.
    pub fn bind(&mut self, root: &PrivateRoot) -> Result<(), Error> {
        self.bind_uid(root.directory().metadata()?.uid())
    }
    fn bind_uid(&mut self, uid: u32) -> Result<(), Error> {
        match self.owner {
            Owner::Unbound(seen) if uid != 0 && seen.is_none_or(|seen| seen == uid) => {
                self.owner = Owner::Bound(uid);
                Ok(())
            }
            _ => Err(Error::Owner),
        }
    }
    /// Checks every ancestor and the final file by symlink_metadata, opens it
    /// read-only, then checks the opened file and its identity again.
    pub fn open(&mut self, path: &str, role: Role) -> Result<File, Error> {
        values::absolute_path(path).map_err(|_| Error::Path)?;
        if role != Role::Configuration && matches!(self.owner, Owner::Unbound(_)) {
            return Err(Error::Unbound);
        }
        let mut owner = self.owner;
        for ancestor in Path::new(path).ancestors().skip(1) {
            let metadata = fs::symlink_metadata(ancestor)?;
            check_directory(
                metadata.is_dir(),
                metadata.uid(),
                metadata.mode(),
                &mut owner,
            )?;
        }
        let before = fs::symlink_metadata(path)?;
        check(&before, role, &mut owner)?;
        let file = File::open(path)?;
        let after = file.metadata()?;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(Error::Changed);
        }
        check(&after, role, &mut owner)?;
        self.record(after.dev(), after.ino(), role == Role::Secret)?;
        self.owner = owner;
        Ok(file)
    }
    fn record(&mut self, device: u64, inode: u64, secret: bool) -> Result<(), Error> {
        let seen = self.seen.get(..self.count).ok_or(Error::Capacity)?;
        if let Some(prior) = seen
            .iter()
            .find(|seen| seen.device == device && seen.inode == inode)
        {
            return if prior.secret == secret {
                Ok(())
            } else {
                Err(Error::Aliased)
            };
        }
        *self.seen.get_mut(self.count).ok_or(Error::Capacity)? = Seen {
            device,
            inode,
            secret,
        };
        self.count += 1;
        Ok(())
    }
}

fn check(metadata: &Metadata, role: Role, owner: &mut Owner) -> Result<(), Error> {
    check_file(
        metadata.is_file(),
        metadata.uid(),
        metadata.mode(),
        role,
        owner,
    )
}
// Group/other write includes sticky shared directories such as /tmp.
fn check_directory(directory: bool, uid: u32, mode: u32, owner: &mut Owner) -> Result<(), Error> {
    if !directory {
        return Err(Error::Type);
    }
    owner.admit(uid)?;
    if mode & 0o022 != 0 {
        return Err(Error::Writable);
    }
    Ok(())
}
fn check_file(
    regular: bool,
    uid: u32,
    mode: u32,
    role: Role,
    owner: &mut Owner,
) -> Result<(), Error> {
    if !regular {
        return Err(Error::Type);
    }
    owner.admit(uid)?;
    if mode & 0o022 != 0 {
        return Err(Error::Writable);
    }
    if mode & 0o7111 != 0 {
        return Err(Error::Mode);
    }
    if role == Role::Secret && !matches!(mode & 0o777, 0o400 | 0o600) {
        return Err(Error::PrivateMode);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn owner_binds_once_to_the_one_nonroot_uid_already_admitted() {
        let mut owner = Owner::Unbound(None);
        owner.admit(0).unwrap();
        owner.admit(1000).unwrap();
        owner.admit(1000).unwrap();
        assert_eq!(owner.admit(1001), Err(Error::Owner));
        let mut inputs = Inputs {
            owner,
            ..Inputs::new()
        };
        assert_eq!(inputs.bind_uid(1001), Err(Error::Owner));
        assert_eq!(inputs.bind_uid(0), Err(Error::Owner));
        inputs.bind_uid(1000).unwrap();
        assert_eq!(inputs.bind_uid(1000), Err(Error::Owner));
        let mut bound = inputs.owner;
        bound.admit(0).unwrap();
        assert_eq!(bound.admit(1001), Err(Error::Owner));

        let mut root_only = Inputs::new();
        root_only.owner.admit(0).unwrap();
        root_only.bind_uid(7).unwrap();
    }

    #[test]
    fn file_modes_follow_role_and_refuse_write_execute_and_special_bits() {
        let mut owner = Owner::Bound(7);
        for role in [Role::Configuration, Role::Public, Role::Secret] {
            for mode in [0o100400, 0o100600] {
                check_file(true, 7, mode, role, &mut owner).unwrap();
            }
            check_file(true, 0, 0o100400, role, &mut owner).unwrap();
            assert_eq!(
                check_file(false, 7, 0o600, role, &mut owner),
                Err(Error::Type)
            );
            assert_eq!(
                check_file(true, 8, 0o600, role, &mut owner),
                Err(Error::Owner)
            );
            for mode in [0o620, 0o602, 0o622] {
                assert_eq!(
                    check_file(true, 7, mode, role, &mut owner),
                    Err(Error::Writable)
                );
            }
            for mode in [0o700, 0o610, 0o601, 0o4600, 0o2600, 0o1600] {
                assert_eq!(
                    check_file(true, 7, mode, role, &mut owner),
                    Err(Error::Mode)
                );
            }
        }
        for mode in [0o640, 0o604, 0o644, 0o444] {
            check_file(true, 7, mode, Role::Public, &mut owner).unwrap();
            check_file(true, 7, mode, Role::Configuration, &mut owner).unwrap();
            assert_eq!(
                check_file(true, 7, mode, Role::Secret, &mut owner),
                Err(Error::PrivateMode)
            );
        }
        for mode in [0o200, 0o000] {
            assert_eq!(
                check_file(true, 7, mode, Role::Secret, &mut owner),
                Err(Error::PrivateMode)
            );
        }
    }

    #[test]
    fn directories_refuse_other_types_owners_and_shared_write() {
        let mut owner = Owner::Bound(7);
        for (uid, mode) in [(0, 0o40755), (7, 0o700), (7, 0o555)] {
            check_directory(true, uid, mode, &mut owner).unwrap();
        }
        assert_eq!(
            check_directory(false, 7, 0o755, &mut owner),
            Err(Error::Type)
        );
        assert_eq!(
            check_directory(true, 8, 0o755, &mut owner),
            Err(Error::Owner)
        );
        for mode in [0o775, 0o757, 0o1777, 0o41777] {
            assert_eq!(
                check_directory(true, 0, mode, &mut owner),
                Err(Error::Writable)
            );
        }
    }

    #[test]
    fn identities_separate_secret_and_public_roles_within_capacity() {
        let mut inputs = Inputs::new();
        inputs.record(1, 2, false).unwrap();
        inputs.record(1, 2, false).unwrap();
        assert_eq!(inputs.record(1, 2, true), Err(Error::Aliased));
        inputs.record(1, 3, true).unwrap();
        inputs.record(1, 3, true).unwrap();
        assert_eq!(inputs.record(1, 3, false), Err(Error::Aliased));
        inputs.record(2, 2, true).unwrap();
        assert_eq!(inputs.count(), 3);
        for inode in 3..u64::try_from(MAX_INPUTS).unwrap() {
            inputs.record(9, inode, false).unwrap();
        }
        assert_eq!(inputs.count(), MAX_INPUTS);
        inputs.record(9, 3, false).unwrap();
        assert_eq!(inputs.record(9, 0, false), Err(Error::Capacity));
        assert_eq!(inputs.count(), MAX_INPUTS);
    }

    #[test]
    fn unbound_inputs_refuse_everything_but_the_configuration() {
        let mut inputs = Inputs::new();
        assert_eq!(
            inputs.open("relative", Role::Configuration).err(),
            Some(Error::Path)
        );
        for role in [Role::Public, Role::Secret] {
            assert_eq!(inputs.open("/", role).err(), Some(Error::Unbound));
        }
        assert_eq!(
            inputs.open("/", Role::Configuration).err(),
            Some(Error::Type)
        );
        assert_eq!(inputs.count(), 0);
    }
}
