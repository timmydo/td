//! Immutable deployment grants; the wire never allocates an application identity.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const O_NOFOLLOW: i32 = 0x20000;
const O_DIRECTORY: i32 = 0x10000;
const O_NONBLOCK: i32 = 0x800;

pub const PORTAL_UID: u32 = 991;

pub const PATH: &str = "/etc/td-bus-applications.tsv";
pub const MAX_BYTES: usize = 64 * 1024;
const MAX_APPS: usize = 256;
const MAX_OWNED: usize = 32;
const HEADER: &str = "td-bus-applications-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub uid: u32,
    pub application: String,
    pub owned: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    owner: u32,
    rules: Vec<Rule>,
}

impl Policy {
    pub fn new(owner: u32, mut rules: Vec<Rule>) -> Result<Self, String> {
        if !(1000..=65533).contains(&owner) || rules.len() > MAX_APPS {
            return Err("invalid application policy owner or row count".into());
        }
        let mut uids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for rule in &mut rules {
            if !(65536..=2147483647).contains(&rule.uid)
                || !application_name(&rule.application)
                || !uids.insert(rule.uid)
                || !names.insert(rule.application.clone())
                || rule.owned.len() > MAX_OWNED
                || rule.owned.iter().any(|name| !owned_name(name))
            {
                return Err("invalid or duplicate application policy entry".into());
            }
            rule.owned.sort();
            if rule
                .owned
                .windows(2)
                .any(|pair| pair.first() == pair.get(1))
            {
                return Err("duplicate application bus grant".into());
            }
        }
        rules.sort_by(|left, right| left.application.cmp(&right.application));
        let policy = Self { owner, rules };
        if policy.to_tsv().len() > MAX_BYTES {
            return Err("application policy exceeds 64 KiB".into());
        }
        Ok(policy)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES || !text.is_ascii() || !text.ends_with('\n') {
            return Err("application policy must be bounded canonical ASCII lines".into());
        }
        let mut lines = text.split_terminator('\n');
        let (header, owner) = lines
            .next()
            .and_then(|line| line.split_once('\t'))
            .ok_or("missing application policy header")?;
        if header != HEADER {
            return Err("unsupported application policy format".into());
        }
        let owner = decimal(owner)?;
        let mut rules = Vec::new();
        for line in lines {
            if rules.len() == MAX_APPS {
                return Err("too many application policy rows".into());
            }
            let fields: Vec<&str> = line.split('\t').take(4).collect();
            let [uid, application, owned] = fields.as_slice() else {
                return Err("invalid application policy row".into());
            };
            let owned = if owned.is_empty() {
                Vec::new()
            } else {
                owned
                    .split(',')
                    .take(MAX_OWNED + 1)
                    .map(str::to_string)
                    .collect()
            };
            rules.push(Rule {
                uid: decimal(uid)?,
                application: (*application).into(),
                owned,
            });
        }
        let policy = Self::new(owner, rules)?;
        if policy.to_tsv() != text {
            return Err("application policy is not canonical".into());
        }
        Ok(policy)
    }

    pub fn to_tsv(&self) -> String {
        let mut text = format!("{HEADER}\t{}\n", self.owner);
        for rule in &self.rules {
            text.push_str(&format!(
                "{}\t{}\t{}\n",
                rule.uid,
                rule.application,
                rule.owned.join(",")
            ));
        }
        text
    }

    pub fn owner(&self) -> u32 {
        self.owner
    }

    pub fn for_uid(&self, uid: u32) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.uid == uid)
    }

    pub fn for_name(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.application == name)
    }

    pub fn admits(&self, uid: u32) -> bool {
        uid == 0 || uid == self.owner || self.is_portal(uid) || self.for_uid(uid).is_some()
    }

    pub fn is_portal(&self, uid: u32) -> bool {
        self.owner == 1000 && uid == PORTAL_UID
    }

    pub fn registration(
        &self,
        uid: u32,
        app: &str,
        services: &[String],
        owned: &[String],
    ) -> Result<(), String> {
        let rule = self
            .for_name(app)
            .ok_or("application is absent from deployment policy")?;
        // Only the assigned kernel identity may register the installed name.
        if uid != rule.uid {
            return Err("kernel UID does not own this application identity".into());
        }
        if !services.is_empty() || owned != rule.owned {
            return Err("registration differs from the immutable application grants".into());
        }
        Ok(())
    }
}

/// The immutable image owns both the policy file and its parent.
pub fn load() -> Result<Policy, String> {
    let directory = Path::new(PATH)
        .parent()
        .ok_or("bus policy has no parent")?;
    load_policy_from(directory, (0, 0))
}

fn load_policy_from(
    directory: &Path,
    owner: (u32, u32),
) -> Result<Policy, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let etc = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(directory)
        .map_err(|error| format!("open bus policy directory: {error}"))?;
    let metadata = etc.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || metadata.mode() & 0o7022 != 0
    {
        return Err("bus policy directory is not root-controlled".into());
    }
    let name = Path::new(PATH)
        .file_name()
        .ok_or("bus policy has no filename")?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(Path::new(&format!("/proc/self/fd/{}", etc.as_raw_fd())).join(name))
        .map_err(|error| format!("open bus application policy: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || metadata.mode() & 0o7777 != 0o444
        || metadata.nlink() != 1
        || metadata.len() > MAX_BYTES as u64
    {
        return Err("bus application policy must be one bounded root-owned 0444 file".into());
    }
    let mut text = String::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read bus application policy: {error}"))?;
    let policy = Policy::parse(&text)?;
    if policy.owner() != 1000 {
        return Err("bus application policy names another session".into());
    }
    Ok(policy)
}

fn decimal(text: &str) -> Result<u32, String> {
    let value = text.parse::<u32>().map_err(|_| "invalid policy UID")?;
    if value.to_string() != text {
        return Err("noncanonical policy UID".into());
    }
    Ok(value)
}

pub(crate) fn application_name(name: &str) -> bool {
    // Stock names must satisfy both the reserved principal and jail grammars.
    name.len() <= 32
        && !name.contains("..")
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

// This source is also compiled by recipes; broker tests cross-check its
// reserved-name decisions against policy::is_reserved_name.
pub(crate) fn owned_name(name: &str) -> bool {
    name.len() <= 255
        && name.contains('.')
        && ![
            "org.freedesktop.DBus",
            "org.freedesktop.portal",
            "org.freedesktop.impl.portal",
        ]
        .contains(&name)
        && !name.starts_with("org.freedesktop.portal.")
        && !name.starts_with("org.freedesktop.impl.portal.")
        && name.split('.').all(|part| {
            part.as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || b"_-".contains(byte))
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn policy_loader_refuses_mutable_redirected_and_noncanonical_files() {
        let root = std::env::temp_dir().join(format!("td-bus-policy-{}-{}", std::process::id(), module_path!()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(&root).unwrap();
        let owner = (metadata.uid(), metadata.gid());
        let path = root.join(Path::new(PATH).file_name().unwrap());
        let valid = "td-bus-applications-v1\t1000\n65536\tfirefox\torg.mozilla.firefox\n";
        assert!(load_policy_from(&root, owner).is_err());
        fs::write(&path, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(load_policy_from(&root, owner).unwrap().to_tsv(), valid);
        assert!(load_policy_from(&root, (owner.0.wrapping_add(1), owner.1)).is_err());
        for mode in [0o644, 0o400, 0o1444] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(load_policy_from(&root, owner).is_err(), "mode {mode:o}");
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let other = root.join("other");
        fs::hard_link(&path, &other).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&other, &path).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_file(&other).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::remove_dir(&path).unwrap();
        for content in [
            valid.replace("1000", "1001"),
            valid.replace("65536", "065536"),
            "x".repeat(MAX_BYTES + 1),
        ] {
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
            assert!(load_policy_from(&root, owner).is_err());
            fs::remove_file(&path).unwrap();
        }
        fs::write(&path, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(load_policy_from(&root, owner).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.with_extension("link");
        symlink(&root, &link).unwrap();
        assert!(load_policy_from(&link, owner).is_err());
        fs::remove_file(link).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

    const TABLE: &str =
        "td-bus-applications-v1\t1000\n65536\tfirefox\torg.mozilla.firefox\n65537\tmail\t\n";

    #[test]
    fn registration_uses_canonical_lists_and_shared_grants_remain_explicit() {
        let policy = Policy::parse(
            "td-bus-applications-v1\t1000\n65536\tone\torg.example.A,org.example.B\n65537\ttwo\torg.example.A\n"
        ).unwrap();
        let sorted = vec!["org.example.A".into(), "org.example.B".into()];
        policy.registration(65536, "one", &[], &sorted).unwrap();
        let reversed = vec!["org.example.B".into(), "org.example.A".into()];
        assert!(policy.registration(65536, "one", &[], &reversed).is_err());
        policy
            .registration(65537, "two", &[], &["org.example.A".into()])
            .unwrap();
    }

    #[test]
    fn canonical_policy_binds_kernel_identity_and_exact_grants() {
        let policy = Policy::parse(TABLE).unwrap();
        assert_eq!(policy.to_tsv(), TABLE);
        let grants = vec!["org.mozilla.firefox".into()];
        policy.registration(65536, "firefox", &[], &grants).unwrap();
        for uid in [0, 991, 992, 1000, 1001, 65537, u32::MAX] {
            assert!(policy.registration(uid, "firefox", &[], &grants).is_err());
        }
        assert!(policy.registration(65536, "mail", &[], &[]).is_err());
        assert!(policy.registration(1000, "absent", &[], &[]).is_err());
        assert!(policy.registration(65536, "firefox", &[], &[]).is_err());
        assert!(policy
            .registration(65536, "firefox", &grants, &grants)
            .is_err());
        for uid in [0, 991, 1000, 65536, 65537] {
            assert!(policy.admits(uid));
        }
        for uid in [992, 993, 1001, 65534, 65538] {
            assert!(!policy.admits(uid));
        }
    }

    #[test]
    fn the_fixed_portal_belongs_only_to_the_stock_session() {
        let policy = Policy::new(1001, Vec::new()).unwrap();
        assert!(!policy.admits(PORTAL_UID));
        assert!(!policy.is_portal(PORTAL_UID));
    }

    #[test]
    fn policy_refuses_aliases_reserved_names_and_noncanonical_encodings() {
        for text in [
            TABLE.replace("65537", "65536"),
            TABLE.replace("\tmail\t", "\tfirefox\t"),
            TABLE.replace("65537", "1000"),
            TABLE.replace("firefox", "app..name"),
            TABLE.replace("firefox", &"a".repeat(33)),
            TABLE.replace("firefox", "App"),
            TABLE.replace("firefox", "1app"),
            TABLE.replace("1000", "01000"),
            TABLE.replace("\tfirefox\t", "\t../firefox\t"),
            TABLE.replace("\n", "\r\n"),
            TABLE.replace("org.mozilla.firefox", "org.freedesktop.portal.Desktop"),
            TABLE.replace("org.mozilla.firefox", "org.mozilla.*"),
            TABLE.replace(
                "org.mozilla.firefox",
                "org.mozilla.firefox,org.mozilla.firefox",
            ),
            TABLE.replace("org.mozilla.firefox", "org.zzz,org.aaa"),
            TABLE.trim_end().into(),
            format!("{TABLE}\n"),
        ] {
            assert!(Policy::parse(&text).is_err(), "{text:?}");
        }
        assert!(Policy::parse(&"x".repeat(MAX_BYTES + 1)).is_err());
    }
}
