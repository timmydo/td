//! Canonical deployment identities; names never allocate Unix credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

#[path = "../../engine/src/principals.rs"]
mod table;
use table::decimal;
pub(crate) use table::{Registry, MAX_BYTES};

const TABLE_NAME: &str = "td-principals.tsv";
pub(crate) const O_NOFOLLOW: i32 = 0x20000;
pub(crate) const O_NONBLOCK: i32 = 0x800;
pub(crate) const O_DIRECTORY: i32 = 0x10000;

fn root_directory(path: &Path, owner: Option<(u32, u32)>) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(path)
        .map_err(|error| format!("open principal configuration directory: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || owner.is_some_and(|owner| owner != (metadata.uid(), metadata.gid()))
        || metadata.mode() & 0o022 != 0
    {
        return Err(
            "principal configuration directories have inconsistent ownership or other writers"
                .into(),
        );
    }
    Ok(file)
}

fn etc_directory(root: &Path, owner: Option<(u32, u32)>) -> Result<File, String> {
    let root = root_directory(root, owner)?;
    let metadata = root.metadata().map_err(|error| error.to_string())?;
    root_directory(
        Path::new(&format!("/proc/self/fd/{}/etc", root.as_raw_fd())),
        Some((metadata.uid(), metadata.gid())),
    )
}

fn read_root_file(etc: &File, name: &str, mode: Option<u32>) -> Result<String, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(format!("/proc/self/fd/{}/{name}", etc.as_raw_fd()))
        .map_err(|error| format!("open principal configuration {name}: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    let owner = etc.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != owner.uid()
        || metadata.gid() != owner.gid()
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o7000 != 0
        || mode.is_some_and(|mode| metadata.mode() & 0o7777 != mode)
        || metadata.nlink() != 1
        || metadata.len() > MAX_BYTES as u64
    {
        return Err(format!(
            "{name} must be one bounded consistently owned regular file with the required mode"
        ));
    }
    let mut text = String::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read principal configuration {name}: {error}"))?;
    if text.len() > MAX_BYTES {
        return Err("principal configuration grew beyond its bound".into());
    }
    Ok(text)
}

pub(crate) fn check_deployment(root: &Path) -> Result<(), String> {
    let etc = etc_directory(root, None)?;
    let registry = Registry::parse(&read_root_file(&etc, TABLE_NAME, Some(0o444))?)?;
    registry.verify_retained_accounts(
        &registry,
        &read_root_file(&etc, "passwd", None)?,
        &read_root_file(&etc, "group", None)?,
        &read_root_file(&etc, "shadow", Some(0o600))?,
    )
}

fn account_rows(text: &str) -> Result<Vec<Vec<&str>>, String> {
    if text.len() > MAX_BYTES || !text.ends_with('\n') || text.contains('\0') || text.contains('\r')
    {
        return Err("account table is not bounded canonical lines".into());
    }
    Ok(text
        .split_terminator('\n')
        .map(|row| row.split(':').take(10).collect())
        .collect())
}

impl Registry {
    pub fn verify_launch_session(
        &self,
        user: &str,
        owner: u32,
        compositor: u32,
    ) -> Result<(), String> {
        let etc = etc_directory(Path::new("/"), Some((0, 0)))?;
        self.launch_session(
            user,
            owner,
            compositor,
            &read_root_file(&etc, "passwd", None)?,
        )
    }

    fn launch_session(
        &self,
        user: &str,
        owner: u32,
        compositor: u32,
        passwd: &str,
    ) -> Result<(), String> {
        if !self
            .session(owner)
            .is_some_and(|session| session.compositor == compositor)
        {
            return Err("launch compositor does not own this session".into());
        }
        let mut found = false;
        for row in account_rows(passwd)? {
            let [name, "x", uid, gid, _, _, _] = row.as_slice() else {
                let name: String = row
                    .first()
                    .copied()
                    .unwrap_or("<missing name>")
                    .chars()
                    .take(32)
                    .collect();
                return Err(format!("invalid launch account record for {name:?}"));
            };
            if *name == user {
                if found
                    || decimal(uid, 1000..=65533)? != owner
                    || decimal(gid, 1000..=65533)? != owner
                {
                    return Err("launch account has the wrong identity".into());
                }
                found = true;
            }
        }
        if !found {
            return Err("launch account is absent".into());
        }
        Ok(())
    }

    pub fn load() -> Result<Self, String> {
        Self::parse(&read_root_file(
            &etc_directory(Path::new("/"), Some((0, 0)))?,
            TABLE_NAME,
            Some(0o444),
        )?)
    }

    pub fn verify_installed_accounts(&self, active: &Self) -> Result<(), String> {
        let etc = etc_directory(Path::new("/"), Some((0, 0)))?;
        self.verify_retained_accounts(
            active,
            &read_root_file(&etc, "passwd", None)?,
            &read_root_file(&etc, "group", None)?,
            &read_root_file(&etc, "shadow", Some(0o600))?,
        )
    }

    fn account_names(&self) -> BTreeMap<u32, String> {
        let mut names = BTreeMap::new();
        for session in self.sessions() {
            for (uid, role) in [
                (session.compositor, "c"),
                (session.broker, "b"),
                (session.portal, "p"),
            ] {
                names.insert(uid, format!("td{role}{}", session.owner));
            }
        }
        for application in self.applications() {
            names.insert(application.uid, format!("tda{}", application.uid));
        }
        names
    }

    #[cfg(test)]
    pub(crate) fn verify_accounts(
        &self,
        passwd: &str,
        group: &str,
        shadow: &str,
    ) -> Result<(), String> {
        self.verify_retained_accounts(self, passwd, group, shadow)
    }

    fn verify_retained_accounts(
        &self,
        active: &Self,
        passwd: &str,
        group: &str,
        shadow: &str,
    ) -> Result<(), String> {
        let names = self.account_names();
        let mut seen_uids = BTreeSet::new();
        let mut seen_names = BTreeSet::new();
        let mut present = BTreeSet::new();
        for row in account_rows(passwd)? {
            let [name, "x", uid, gid, _, _, shell] = row.as_slice() else {
                return Err("invalid principal passwd row".into());
            };
            let uid = decimal(uid, 0..=u32::MAX)?;
            let gid = decimal(gid, 0..=u32::MAX)?;
            if !seen_uids.insert(uid) || !seen_names.insert(*name) {
                return Err("ambiguous account name or uid".into());
            }
            if let Some(expected) = names.get(&uid) {
                if expected != name || gid != uid || *shell != "/bin/false" {
                    return Err("reserved principal uid belongs to another account".into());
                }
                present.insert(*name);
            } else if names.values().any(|expected| expected == name) || names.contains_key(&gid) {
                return Err("another account claims a reserved principal name or gid".into());
            }
        }
        if active
            .sessions()
            .any(|session| !seen_uids.contains(&session.owner))
        {
            return Err("principal session has no human account".into());
        }
        let mut seen_gids = BTreeSet::new();
        let mut group_names = BTreeSet::new();
        for row in account_rows(group)? {
            let [name, "x", gid, members] = row.as_slice() else {
                return Err("invalid principal group row".into());
            };
            let gid = decimal(gid, 0..=u32::MAX)?;
            if !seen_gids.insert(gid) || !group_names.insert(*name) {
                return Err("ambiguous group name or gid".into());
            }
            if let Some(expected) = names.get(&gid) {
                if expected != name || !members.is_empty() {
                    return Err("reserved principal group has another name or members".into());
                }
            } else if names.values().any(|expected| expected == name) {
                return Err("another group claims a reserved principal name".into());
            }
            if members
                .split(',')
                .any(|member| names.values().any(|expected| expected == member))
            {
                return Err("principal account has supplementary group authority".into());
            }
        }
        if present.iter().any(|name| !group_names.contains(name)) {
            return Err("principal account lacks its canonical primary group".into());
        }
        let mut shadow_names = BTreeSet::new();
        for row in account_rows(shadow)? {
            let [name, password, _, _, _, _, _, _, _] = row.as_slice() else {
                return Err("invalid principal shadow row".into());
            };
            if !shadow_names.insert(*name) {
                return Err("ambiguous shadow account".into());
            }
            if names.values().any(|expected| expected == name)
                && (!present.contains(name) || *password != "!td-service")
            {
                return Err(
                    "principal shadow entry lacks an account or permits human login".into(),
                );
            }
        }
        if !present.is_subset(&shadow_names) {
            return Err("principal account lacks service authorization".into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "principals_tests.rs"]
mod tests;
