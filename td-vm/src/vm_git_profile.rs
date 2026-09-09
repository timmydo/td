//! Operator-owned Git configuration; guest keys never enter this profile.
use crate::{io, sha256, vm_git_origin::Origin, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LIMIT: u64 = 8192;
const FILE: &str = "git-profile";
const FIELDS: &[&str] = &[
    "repository",
    "address",
    "port",
    "user",
    "server-uid",
    "socket",
    "registrar",
    "git",
    "host-key",
    "author-name",
    "author-email",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    fields: BTreeMap<String, String>,
}

fn absolute(value: &str) -> Result<&Path> {
    let path = Path::new(value);
    if !path.is_absolute()
        || value.len() > 1024
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("profile paths must be absolute without empty or dot components".into());
    }
    Ok(path)
}

fn key(value: &str) -> Result<()> {
    let encoded = value
        .strip_prefix("ssh-ed25519 ")
        .ok_or("host-key must be one Ed25519 public key without a comment")?;
    // The SSH algorithm/length prefix occupies 25 full base64 characters and
    // the first two bits of character 26. The complete wire blob is 51 bytes.
    let prefix = "AAAAC3NzaC1lZDI1NTE5AAAAI";
    if encoded.len() != 68
        || !encoded.starts_with(prefix)
        || !encoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
        || !matches!(encoded.as_bytes().get(25), Some(b'A'..=b'P'))
    {
        return Err("host-key is not an ordinary Ed25519 public key".into());
    }
    Ok(())
}

impl Profile {
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if text.len() as u64 > LIMIT
            || !text.ends_with('\n')
            || lines.next() != Some("TDVM-GIT-PROFILE-1")
        {
            return Err("invalid Git profile header, size or final newline".into());
        }
        let mut fields = BTreeMap::new();
        for line in lines {
            let (name, value) = line.split_once('=').ok_or("invalid Git profile field")?;
            if !FIELDS.contains(&name)
                || value.is_empty()
                || value.chars().any(char::is_control)
                || fields.insert(name.to_string(), value.to_string()).is_some()
            {
                return Err("unknown, duplicate or malformed Git profile field".into());
            }
        }
        if fields.len() != FIELDS.len() {
            return Err("Git profile is missing required fields".into());
        }
        let profile = Self { fields };
        for field in ["repository", "socket", "registrar", "git"] {
            absolute(profile.get(field)?)?;
        }
        Origin::new(profile.get("repository")?.into(), "a".repeat(40))?;
        let address = profile.get("address")?;
        if address.len() > 253
            || address.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err("address must be an IPv4 address or DNS hostname".into());
        }
        if address.split('.').count() == 4
            && address.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && address.parse::<std::net::Ipv4Addr>().is_err()
        {
            return Err("invalid IPv4 address".into());
        }
        let user = profile.get("user")?;
        if user.len() > 32
            || user.starts_with('-')
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("invalid SSH account name".into());
        }
        let port: u16 = profile
            .get("port")?
            .parse()
            .map_err(|_| "invalid SSH port")?;
        let uid: u32 = profile
            .get("server-uid")?
            .parse()
            .map_err(|_| "invalid registrar UID")?;
        if port == 0
            || port.to_string() != profile.get("port")?
            || uid == u32::MAX
            || uid.to_string() != profile.get("server-uid")?
        {
            return Err("port and UID must be canonical decimal values".into());
        }
        if profile.get("socket")?.len() + "/control".len() > 107 {
            return Err("registrar socket pathname is too long".into());
        }
        key(profile.get("host-key")?)?;
        for field in ["author-name", "author-email"] {
            let value = profile.get(field)?;
            if value.len() > 200 || value.trim() != value || value.contains(['<', '>']) {
                return Err("invalid Git author identity".into());
            }
        }
        Ok(profile)
    }

    fn get(&self, name: &str) -> Result<&str> {
        self.fields
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {name}"))
    }

    pub fn repository(&self) -> Result<&str> {
        self.get("repository")
    }

    pub fn encode(&self) -> String {
        let mut text = String::from("TDVM-GIT-PROFILE-1\n");
        for (name, value) in &self.fields {
            text.push_str(&format!("{name}={value}\n"));
        }
        text
    }

    pub fn fingerprint(&self) -> String {
        sha256::hex_digest(self.encode().as_bytes())
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let git = trusted_program(self.get("git")?)?;
        let mut command = Command::new(git);
        command
            .env_clear()
            .current_dir("/")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("--no-replace-objects")
            .arg("--git-dir")
            .arg(self.get("repository")?)
            .args(args);
        let bytes = capture(command)?;
        let text = String::from_utf8(bytes).map_err(|_| "non-UTF-8 Git reply")?;
        Ok(text.trim_end_matches('\n').into())
    }

    pub fn clone_plan(&self, id: &str, branch: &str, commit: &str, key: &str) -> Result<crate::vm_wire::workspace::Plan> {
        let plan = crate::vm_wire::workspace::Plan {
            id: id.into(), branch: branch.into(), commit: commit.into(),
            repository: self.repository()?.into(), address: self.get("address")?.into(),
            port: self.get("port")?.parse().map_err(|_| "invalid SSH port")?,
            user: self.get("user")?.into(), host_key: self.get("host-key")?.into(), guest_key: key.into(),
            author_name: self.get("author-name")?.into(), author_email: self.get("author-email")?.into(),
        };
        crate::vm_wire::workspace::Plan::parse(&plan.encode())
    }

    pub fn enroll(&self, id: &str, branch: &str, key: &str, lock: &File) -> Result<()> {
        if !crate::vm_git_names::instance_valid(id) || !crate::vm_git_names::branch_valid(branch) {
            return Err("invalid workspace enrollment identity or branch".into());
        }
        let key = crate::vm_wire::git_key::key(key)?;
        self.change(&["enroll", self.repository()?, id, branch, key], lock)
    }

    pub fn revoke(&self, id: &str, lock: &File) -> Result<()> {
        if !crate::vm_git_names::instance_valid(id) { return Err("invalid workspace revocation identity".into()); }
        self.check()?;
        self.change(&["revoke", self.repository()?, id], lock)
    }

    pub fn start(&self, id: &str, branch: &str, expected: Option<&str>, lock: &File) -> Result<String> {
        if !crate::vm_git_names::instance_valid(id) || !crate::vm_git_names::branch_valid(branch) {
            return Err("invalid starting-commit identity or branch".into());
        }
        if let Some(oid) = expected { Origin::new(self.repository()?.into(), oid.into())?; }
        let output = self.request(&["start", self.repository()?, id, branch, expected.unwrap_or("main")], lock)?;
        let origin = Origin::parse(std::str::from_utf8(&output).map_err(|_| "invalid starting-commit reply")?)?;
        if origin.repository != self.repository()? || expected.is_some_and(|oid| oid != origin.head) {
            return Err("registrar starting commit differs from the workspace".into());
        }
        Ok(origin.head)
    }

    fn change(&self, args: &[&str], lock: &File) -> Result<()> {
        if !self.request(args, lock)?.is_empty() { return Err("unexpected registrar mutation output".into()); }
        Ok(())
    }

    fn request(&self, args: &[&str], lock: &File) -> Result<Vec<u8>> {
        let registrar = trusted_program(self.get("registrar")?)?;
        let mut command = Command::new(registrar);
        command.env_clear().current_dir("/").arg("request")
            .arg(self.get("socket")?).arg(self.get("server-uid")?).args(args);
        capture_input(command, Stdio::from(io(lock.try_clone(), "retain instance lock in registrar client")?))
    }

    pub fn check(&self) -> Result<Origin> {
        let repository = io(
            fs::canonicalize(self.get("repository")?),
            "resolve Git origin",
        )?;
        if repository.to_str() != Some(self.get("repository")?) {
            return Err("repository must name its canonical absolute path".into());
        }
        if self.git(&["rev-parse", "--is-bare-repository"])? != "true"
            || self.git(&["symbolic-ref", "HEAD"])? != "refs/heads/main"
        {
            return Err("Git origin must be bare with main as its default branch".into());
        }
        // Validate the local commit; the independently sampled registrar tip
        // can differ if main moves between these two read-only probes.
        Origin::new(
            self.get("repository")?.into(),
            self.git(&["rev-parse", "--verify", "HEAD^{commit}"])?,
        )?;
        let registrar = trusted_program(self.get("registrar")?)?;
        let mut command = Command::new(registrar);
        command
            .env_clear()
            .current_dir("/")
            .arg("request")
            .arg(self.get("socket")?)
            .arg(self.get("server-uid")?)
            .arg("origin");
        let output = capture(command)?;
        let origin =
            Origin::parse(std::str::from_utf8(&output).map_err(|_| "invalid registrar output")?)?;
        if origin.repository != self.get("repository")? {
            return Err("registrar serves a different repository than the Git profile".into());
        }
        // main may move between the local and registrar reads. Its latest
        // reported tip is information, not a retained provisioning anchor.
        Ok(origin)
    }
}

fn trusted_program(value: &str) -> Result<PathBuf> {
    let uid = io(fs::metadata("/proc/self"), "inspect current UID")?.uid();
    let path = io(
        fs::canonicalize(absolute(value)?),
        "resolve profile executable",
    )?;
    let metadata = io(fs::symlink_metadata(&path), "inspect profile executable")?;
    if !metadata.is_file()
        || ![0, uid].contains(&metadata.uid())
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err("profile executable must be a trusted executable file".into());
    }
    for parent in path.ancestors().skip(1) {
        let metadata = io(fs::symlink_metadata(parent), "inspect executable ancestor")?;
        if !metadata.is_dir()
            || ![0, uid].contains(&metadata.uid())
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err("untrusted profile executable ancestor".into());
        }
    }
    Ok(path)
}

fn capture(command: Command) -> Result<Vec<u8>> {
    capture_input(command, Stdio::null())
}

fn capture_input(mut command: Command, input: Stdio) -> Result<Vec<u8>> {
    let mut child = io(
        command
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn(),
        "start Git profile probe",
    )?;
    let result = (|| {
        let mut bytes = Vec::new();
        let stdout = child.stdout.take().ok_or("missing probe output")?;
        io(
            stdout.take(LIMIT + 1).read_to_end(&mut bytes),
            "read profile probe",
        )?;
        if bytes.len() as u64 > LIMIT {
            return Err("profile probe output exceeds limit".into());
        }
        Ok(bytes)
    })();
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    if !io(child.wait(), "wait for profile probe")?.success() {
        return Err("Git profile probe failed; inspect its diagnostics".into());
    }
    Ok(bytes)
}

pub fn read(path: &Path, private: bool) -> Result<Profile> {
    let file = io(
        OpenOptions::new()
            .read(true)
            // Linux x86-64 O_NOFOLLOW | O_NONBLOCK; inspect before reading.
            .custom_flags(0x20000 | 0x800)
            .open(path),
        "open Git profile",
    )?;
    let meta = io(file.metadata(), "inspect Git profile")?;
    let uid = io(fs::metadata("/proc/self"), "inspect current UID")?.uid();
    if !meta.is_file()
        || ![0, uid].contains(&meta.uid())
        || meta.nlink() != 1
        || meta.mode() & (if private { 0o077 } else { 0o022 }) != 0
    {
        return Err(
            "Git profile must be a trusted regular file with restricted permissions".into(),
        );
    }
    let mut text = String::new();
    io(
        file.take(LIMIT + 1).read_to_string(&mut text),
        "read Git profile",
    )?;
    Profile::parse(&text)
}

pub fn load(root: &Path) -> Result<Profile> {
    read(&root.join(FILE), true)
}

pub fn optional(root: &Path) -> Result<Option<Profile>> {
    match fs::symlink_metadata(root.join(FILE)) {
        Ok(_) => load(root).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("inspect Git profile: {e}")),
    }
}

pub fn configure(root: &Path, input: &Path) -> Result<()> {
    let profile = read(input, false)?;
    // The caller holds the stable profile lock. A stale staging file can be
    // removed only under that same lock; the published generation stays intact.
    let temporary = root.join("git-profile.tmp");
    match fs::symlink_metadata(&temporary) {
        Ok(meta) if meta.is_file() => {
            io(
                fs::remove_file(&temporary),
                "remove interrupted profile staging",
            )?;
        }
        Ok(_) => return Err("unexpected Git profile staging entry".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect profile staging: {error}")),
    }
    let result = (|| {
        let mut file = io(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary),
            "stage Git profile",
        )?;
        io(
            file.write_all(profile.encode().as_bytes()),
            "write Git profile",
        )?;
        io(file.sync_all(), "sync Git profile")?;
        io(
            fs::rename(&temporary, root.join(FILE)),
            "publish Git profile",
        )?;
        io(
            File::open(root).and_then(|file| file.sync_all()),
            "sync profile directory",
        )?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
    fn example() -> String {
        format!("TDVM-GIT-PROFILE-1\nrepository=/srv/git/td.git\naddress=10.0.2.2\nport=22\nuser=test\nserver-uid=1001\nsocket=/home/test/.td-vm-registrar\nregistrar=/usr/local/libexec/td-vm-registrar\ngit=/bin/git\nhost-key={KEY}\nauthor-name=Timmy Douglas\nauthor-email=mail@timmydouglas.com\n")
    }
    #[test]
    fn profile_roundtrip_and_invalid_fields() {
        let text = example();
        let p = Profile::parse(&text).unwrap();
        assert_eq!(Profile::parse(&p.encode()).unwrap(), p);
        assert_eq!(p.fingerprint().len(), 64);
        for bad in [
            text.replace("port=22", "port=022"),
            text.replace("port=22", "port=0"),
            text.replace("address=10.0.2.2", "address=host;sh"),
            text.replace("address=10.0.2.2", "address=999.999.999.999"),
            text.replace("address=10.0.2.2", "address=10.00.2.2"),
            text.replace("user=test", "user=-oProxyCommand"),
            text.replace("repository=/srv/git/td.git", "repository=/srv/../td.git"),
            text.replace("server-uid=1001", "server-uid=4294967295"),
            text.replace(KEY, "ssh-rsa invalid"),
            format!("{text}port=23\n"),
            format!("{text}private-key=/secret\n"),
            text.trim_end().into(),
            text.replace("author-name=Timmy Douglas\n", ""),
        ] {
            assert!(Profile::parse(&bad).is_err(), "{bad}");
        }
    }
}
