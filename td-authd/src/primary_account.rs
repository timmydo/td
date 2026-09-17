//! The single human filesystem identity, shared by grant creation and admission.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const PASSWD: &str = "/etc/passwd";
const LIMIT: usize = 64 * 1024;
const MAX_RECORDS: usize = 1024;
pub(crate) const UID: u32 = 1000;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PrimaryAccount {
    name: String,
}

impl PrimaryAccount {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn home(&self) -> PathBuf {
        Path::new("/home").join(self.name())
    }

    pub(crate) fn persistent_home(&self) -> PathBuf {
        Path::new("/var/home").join(self.name())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn number(value: &str) -> io::Result<u32> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(
            "account id is not a canonical unsigned decimal integer",
        ));
    }
    value.parse().map_err(|_| invalid("account id exceeds u32"))
}

pub(crate) fn parse(text: &str) -> io::Result<PrimaryAccount> {
    if text.is_empty()
        || text.len() > LIMIT
        || !text.ends_with('\n')
        || text
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\n')
    {
        return Err(invalid(
            "account database must be bounded newline-terminated text",
        ));
    }
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut primary = None;
    let mut fields = Vec::with_capacity(7);
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_RECORDS || line.len() > 4096 {
            return Err(invalid("account database exceeds its record bound"));
        }
        fields.clear();
        fields.extend(line.split(':'));
        let [name, _, uid, gid, _, home, shell] = fields.as_slice() else {
            return Err(invalid("account record requires seven fields"));
        };
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || !names.insert(*name)
        {
            return Err(invalid("invalid or duplicate account name"));
        }
        let uid = number(uid)?;
        let gid = number(gid)?;
        if !ids.insert(uid) {
            return Err(invalid("two account names claim one uid"));
        }
        if !home.starts_with('/') || !shell.starts_with('/') {
            return Err(invalid("account home and shell must be absolute"));
        }
        if uid != UID {
            continue;
        }
        let account = PrimaryAccount {
            name: (*name).to_owned(),
        };
        if gid != UID
            || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
            })
            || ![account.home(), account.persistent_home()]
                .iter()
                .any(|expected| expected.to_str() == Some(*home))
        {
            return Err(invalid(
                "primary account requires gid 1000 and its canonical named home",
            ));
        }
        primary = Some(account);
    }
    primary.ok_or_else(|| invalid("account database has no uid-1000 human"))
}

fn read(path: &Path, owner: u32) -> io::Result<PrimaryAccount> {
    // /etc is deployment-owned and account publication precedes sessions.
    // Match the current image's no-symlink account-file contract.
    let before = fs::symlink_metadata(path)?;
    if !before.is_file()
        || before.uid() != owner
        || before.mode() & 0o7777 != 0o644
        || before.len() > LIMIT as u64
    {
        return Err(invalid(
            "account database is not a bounded mode-0644 file with the required owner",
        ));
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if (
        before.dev(),
        before.ino(),
        before.uid(),
        before.mode(),
        before.len(),
    ) != (
        opened.dev(),
        opened.ino(),
        opened.uid(),
        opened.mode(),
        opened.len(),
    ) {
        return Err(invalid("account database changed while opening"));
    }
    let mut text = String::new();
    file.take((LIMIT + 1) as u64).read_to_string(&mut text)?;
    if text.len() as u64 != opened.len() {
        return Err(invalid("account database changed while reading"));
    }
    parse(&text)
}

pub(crate) fn load() -> io::Result<PrimaryAccount> {
    read(Path::new(PASSWD), 0).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("read primary account from {PASSWD}: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PASSWD_TEXT: &str = "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000:Alice:/home/alice:/bin/sh\ntda65536:x:65536:65536:application:/var/lib/td/applications/65536:/bin/false\n";

    #[test]
    fn selected_identity_drives_the_home_without_changing_the_numeric_owner() {
        for name in ["alice", "tester", "user_2", "a-b"] {
            let account = parse(&PASSWD_TEXT.replace("alice", name)).unwrap();
            assert_eq!(account.name(), name);
            assert_eq!(account.persistent_home(), Path::new("/var/home").join(name));
        }
        assert_eq!(
            parse(&PASSWD_TEXT.replace("/home/alice", "/var/home/alice")).unwrap(),
            parse(PASSWD_TEXT).unwrap()
        );
    }

    #[test]
    fn aliases_malformed_unrelated_rows_and_escaping_home_choices_refuse() {
        for text in [
            PASSWD_TEXT.replace("alice", "root"),
            format!("{PASSWD_TEXT}alias:x:1000:1000:Alias:/home/alias:/bin/sh\n"),
            format!("{PASSWD_TEXT}alias:x:65536:65536:Alias:/home/alias:/bin/sh\n"),
            PASSWD_TEXT.replace("1000:1000", "1000:0"),
            PASSWD_TEXT.replace("1000:1000", "1001:1001"),
            PASSWD_TEXT.replace("alice", "../root"),
            PASSWD_TEXT.replace("alice", "-alice"),
            PASSWD_TEXT.replace("alice", "Alice"),
            PASSWD_TEXT.replace("alice", &"a".repeat(33)),
            PASSWD_TEXT.replace("/home/alice", "home/alice"),
            PASSWD_TEXT.replace("/bin/false", "bin/false"),
            PASSWD_TEXT.replace("1000:1000", "+1000:1000"),
            PASSWD_TEXT.replace("1000:1000", "01000:1000"),
            PASSWD_TEXT.replace("1000:1000", "1000:01000"),
            PASSWD_TEXT.replace("Alice", "Alice\r"),
            PASSWD_TEXT.replace("Alice", &"a".repeat(4096)),
            format!(
                "{PASSWD_TEXT}{}",
                (0..MAX_RECORDS)
                    .map(|i| format!("u{i}:x:{}:1::/:/bin/false\n", i + 2000))
                    .collect::<String>()
            ),
            PASSWD_TEXT.replace("/home/alice", "/var/lib/td/secrets"),
            PASSWD_TEXT.replace("/home/alice", "/home/alice/../root"),
            PASSWD_TEXT.replace("/home/alice", "/home/alice/"),
            PASSWD_TEXT.replace("65536:65536", "bad:65536"),
            PASSWD_TEXT.replace("65536:65536", "65536:4294967296"),
            format!("{PASSWD_TEXT}broken\n"),
            PASSWD_TEXT.trim_end().to_owned(),
            PASSWD_TEXT.replace("Alice", "Alice\0"),
            "x".repeat(LIMIT + 1),
            String::new(),
        ] {
            assert!(parse(&text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn file_read_requires_bounded_regular_owned_data() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "td-primary-account-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("passwd");
        fs::write(&path, PASSWD_TEXT).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let owner = fs::metadata(&path).unwrap().uid();
        assert_eq!(read(&path, owner).unwrap().name(), "alice");
        assert!(read(&path, owner.wrapping_add(1)).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read(&path, owner).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, [0xff, b'\n']).unwrap();
        assert!(read(&path, owner).is_err());
        fs::write(&path, vec![b'x'; LIMIT + 1]).unwrap();
        assert!(read(&path, owner).is_err());
        assert!(read(&root, owner).is_err());
        fs::write(&path, PASSWD_TEXT).unwrap();
        let link = root.join("active-passwd");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link, owner).is_err());
        fs::remove_file(&path).unwrap();
        assert!(read(&link, owner).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
