//! Canonical deployment identities; names never allocate Unix credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const MAGIC: &str = "td-principals-v1\n";
pub(crate) const MAX_BYTES: usize = 64 * 1024;
const MAX_ROWS: usize = 256;
const TABLE_NAME: &str = "td-principals.tsv";
pub(crate) const O_NOFOLLOW: i32 = 0x20000;
pub(crate) const O_NONBLOCK: i32 = 0x800;
pub(crate) const O_DIRECTORY: i32 = 0x10000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub owner: u32,
    pub compositor: u32,
    pub broker: u32,
    pub portal: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Application {
    pub owner: u32,
    pub name: String,
    pub uid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Registry {
    sessions: BTreeMap<u32, Session>,
    applications: BTreeMap<(u32, String), Application>,
}

fn decimal(value: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    let number = value.parse::<u32>().map_err(|_| "invalid principal uid")?;
    if !range.contains(&number) || number.to_string() != value {
        return Err("noncanonical or out-of-range principal uid".into());
    }
    Ok(number)
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

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
            .sessions
            .get(&owner)
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
        for session in self.sessions.values() {
            for (uid, role) in [
                (session.compositor, "c"),
                (session.broker, "b"),
                (session.portal, "p"),
            ] {
                names.insert(uid, format!("td{role}{}", session.owner));
            }
        }
        for application in self.applications.values() {
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
            .sessions
            .keys()
            .any(|owner| !seen_uids.contains(owner))
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

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES || !text.ends_with('\n') || !text.is_ascii() {
            return Err("principal table is oversized or not canonical ASCII lines".into());
        }
        let rows = text
            .strip_prefix(MAGIC)
            .ok_or("unsupported principal table format")?;
        let mut registry = Self {
            sessions: BTreeMap::new(),
            applications: BTreeMap::new(),
        };
        let mut assigned = BTreeSet::new();
        let mut previous_session = None;
        let mut previous_application = None;
        for (index, row) in rows.split_terminator('\n').enumerate() {
            if index >= MAX_ROWS {
                return Err("principal table exceeds 256 rows".into());
            }
            let fields: Vec<&str> = row.split('\t').take(6).collect();
            match fields.as_slice() {
                ["session", owner, compositor, broker, portal] => {
                    let owner = decimal(owner, 1000..=65533)?;
                    let compositor = decimal(compositor, 1..=999)?;
                    let broker = decimal(broker, 1..=999)?;
                    let portal = decimal(portal, 1..=999)?;
                    if previous_application.is_some()
                        || previous_session.is_some_and(|previous| previous >= owner)
                    {
                        return Err("principal sessions are duplicate or unsorted".into());
                    }
                    for uid in [compositor, broker, portal] {
                        if !assigned.insert(uid) {
                            return Err("principal uid is assigned more than once".into());
                        }
                    }
                    registry.sessions.insert(
                        owner,
                        Session {
                            owner,
                            compositor,
                            broker,
                            portal,
                        },
                    );
                    previous_session = Some(owner);
                }
                ["application", owner, application, uid] => {
                    let owner = decimal(owner, 1000..=65533)?;
                    let uid = decimal(uid, 65536..=2147483647)?;
                    if !registry.sessions.contains_key(&owner) {
                        return Err("application has no declared human session".into());
                    }
                    if !name(application) {
                        return Err("invalid principal application name".into());
                    }
                    let key = (owner, (*application).to_string());
                    if previous_application
                        .as_ref()
                        .is_some_and(|previous| previous >= &key)
                    {
                        return Err("principal applications are duplicate or unsorted".into());
                    }
                    if !assigned.insert(uid) {
                        return Err("principal uid is assigned more than once".into());
                    }
                    registry.applications.insert(
                        key.clone(),
                        Application {
                            owner,
                            name: (*application).to_string(),
                            uid,
                        },
                    );
                    previous_application = Some(key);
                }
                _ => return Err("invalid principal row shape".into()),
            }
        }
        if registry.sessions.is_empty() {
            return Err("principal table needs at least one session".into());
        }
        Ok(registry)
    }

    #[cfg(test)]
    pub fn session(&self, owner: u32) -> Option<&Session> {
        self.sessions.get(&owner)
    }

    #[cfg(test)]
    pub fn application(&self, owner: u32, name: &str) -> Option<&Application> {
        self.applications.get(&(owner, name.to_string()))
    }

    #[cfg(test)]
    pub fn application_for_uid(&self, uid: u32) -> Option<&Application> {
        self.applications
            .values()
            .find(|application| application.uid == uid)
    }

    /// Preserve retired assignments so a later deployment cannot recycle them.
    pub fn enroll(&self, desired: &Self) -> Result<Self, String> {
        let mut retained = self.clone();
        for (owner, session) in &desired.sessions {
            if self
                .sessions
                .get(owner)
                .is_some_and(|prior| prior != session)
            {
                return Err("deployment changes an enrolled session identity".into());
            }
            retained.sessions.insert(*owner, session.clone());
        }
        for (key, application) in &desired.applications {
            if self
                .applications
                .get(key)
                .is_some_and(|prior| prior != application)
            {
                return Err("deployment changes an enrolled application identity".into());
            }
            retained
                .applications
                .insert(key.clone(), application.clone());
        }
        // Validate aggregate bounds and cross-principal collisions in the union.
        Self::parse(&retained.encode())
    }

    pub fn encode(&self) -> String {
        let mut text = MAGIC.to_string();
        for session in self.sessions.values() {
            text.push_str(&format!(
                "session\t{}\t{}\t{}\t{}\n",
                session.owner, session.compositor, session.broker, session.portal
            ));
        }
        for application in self.applications.values() {
            text.push_str(&format!(
                "application\t{}\t{}\t{}\n",
                application.owner, application.name, application.uid
            ));
        }
        text
    }
}

#[cfg(test)]
#[path = "principals_tests.rs"]
mod tests;
