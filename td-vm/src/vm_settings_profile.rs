//! Operator-selected agent settings sources. Credential stores are excluded.
use crate::{io, sha256, vm_git_profile, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const PROFILE_LIMIT: u64 = 16 * 1024;
const SETTINGS_LIMIT: u64 = 64 * 1024;
// Linux x86-64 O_NOFOLLOW | O_NONBLOCK; inspect opened objects before use.
const OPEN_READ_FLAGS: i32 = 0x20000 | 0x800;
const FILE: &str = "settings-profile";
const FIELDS: &[&str] = &[
    "default-agent",
    "workspace",
    "path",
    "codex-home",
    "codex",
    "codex-sha256",
    "codex-version",
    "codex-config-sha256",
    "claude-home",
    "claude",
    "claude-sha256",
    "claude-version",
    "claude-settings-sha256",
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
        return Err(
            "settings profile paths must be absolute without empty or dot components".into(),
        );
    }
    Ok(path)
}

fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("settings fingerprints must be lowercase SHA-256 values".into());
    }
    Ok(())
}

fn trusted_directory(value: &str, label: &str, boundary: Option<&Path>) -> Result<PathBuf> {
    let uid = io(fs::metadata("/proc/self"), "inspect current UID")?.uid();
    let path = io(
        fs::canonicalize(absolute(value)?),
        &format!("resolve {label}"),
    )?;
    if path.to_str() != Some(value) {
        return Err(format!("{label} must name its canonical absolute path"));
    }
    let metadata = io(fs::symlink_metadata(&path), &format!("inspect {label}"))?;
    if !metadata.is_dir() || ![0, uid].contains(&metadata.uid()) || metadata.mode() & 0o022 != 0 {
        return Err(format!("{label} must be a trusted non-writable directory"));
    }
    let mut reached_boundary = boundary.is_none();
    for ancestor in path.ancestors().skip(1) {
        let metadata = io(
            fs::symlink_metadata(ancestor),
            &format!("inspect {label} ancestor"),
        )?;
        if !metadata.is_dir()
            || ![0, uid].contains(&metadata.uid())
            || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
        {
            return Err(format!("untrusted {label} ancestor"));
        }
        if boundary == Some(ancestor) {
            reached_boundary = true;
            break;
        }
    }
    if !reached_boundary {
        return Err(format!("{label} is outside its trust boundary"));
    }
    Ok(path)
}

fn settings(path: &Path, label: &str) -> Result<Vec<u8>> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_READ_FLAGS)
            .open(path),
        &format!("open {label}"),
    )?;
    let uid = io(fs::metadata("/proc/self"), "inspect current UID")?.uid();
    let metadata = io(file.metadata(), &format!("inspect {label}"))?;
    if !metadata.is_file()
        || ![0, uid].contains(&metadata.uid())
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
    {
        return Err(format!(
            "{label} must be a trusted singly linked regular file"
        ));
    }
    let mut bytes = Vec::new();
    io(
        file.take(SETTINGS_LIMIT + 1).read_to_end(&mut bytes),
        &format!("read {label}"),
    )?;
    if bytes.len() as u64 > SETTINGS_LIMIT {
        return Err(format!("{label} exceeds 64 KiB"));
    }
    Ok(bytes)
}

fn executable_fingerprint(path: &Path, label: &str) -> Result<String> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_READ_FLAGS)
            .open(path),
        &format!("open {label}"),
    )?;
    let opened = io(file.metadata(), &format!("inspect opened {label}"))?;
    let current = io(fs::symlink_metadata(path), &format!("inspect {label}"))?;
    if !opened.is_file()
        || opened.dev() != current.dev()
        || opened.ino() != current.ino()
        || opened.nlink() != 1
    {
        return Err(format!("{label} changed while it was being inspected"));
    }
    io(sha256::sha256_reader(file), &format!("fingerprint {label}"))
}

fn path_list(value: &str, boundary: Option<&Path>) -> Result<()> {
    if value.len() > 4096 || value.is_empty() {
        return Err("settings profile PATH is empty or exceeds its limit".into());
    }
    for entry in value.split(':') {
        trusted_directory(entry, "settings profile PATH entry", boundary)?;
    }
    Ok(())
}

fn trusted_program(value: &str, boundary: Option<&Path>) -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(boundary) = boundary {
        return vm_git_profile::trusted_program_beneath(value, boundary);
    }
    let _ = boundary;
    vm_git_profile::trusted_program(value)
}

impl Profile {
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if text.len() as u64 > PROFILE_LIMIT
            || !text.ends_with('\n')
            || lines.next() != Some("TDVM-SETTINGS-PROFILE-1")
        {
            return Err("invalid settings profile header, size or final newline".into());
        }
        let mut fields = BTreeMap::new();
        for line in lines {
            let (name, value) = line
                .split_once('=')
                .ok_or("invalid settings profile field")?;
            if !FIELDS.contains(&name)
                || value.is_empty()
                || value.chars().any(char::is_control)
                || fields.insert(name.into(), value.into()).is_some()
            {
                return Err("unknown, duplicate or malformed settings profile field".into());
            }
        }
        if fields.len() != FIELDS.len() {
            return Err("settings profile is missing required fields".into());
        }
        let profile = Self { fields };
        if !matches!(profile.get("default-agent")?, "codex" | "claude") {
            return Err("default-agent must be codex or claude".into());
        }
        for field in ["workspace", "codex-home", "codex", "claude-home", "claude"] {
            absolute(profile.get(field)?)?;
        }
        let path = profile.get("path")?;
        if path.len() > 4096
            || path.is_empty()
            || path.split(':').any(|entry| absolute(entry).is_err())
        {
            return Err(
                "settings profile PATH must contain only absolute canonical-shaped entries".into(),
            );
        }
        for field in ["codex-version", "claude-version"] {
            let value = profile.get(field)?;
            if value.len() > 256 || value.trim() != value {
                return Err("settings profile versions must be one bounded line".into());
            }
        }
        for field in [
            "codex-sha256",
            "codex-config-sha256",
            "claude-sha256",
            "claude-settings-sha256",
        ] {
            digest(profile.get(field)?)?;
        }
        Ok(profile)
    }

    fn get(&self, name: &str) -> Result<&str> {
        self.fields
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {name}"))
    }

    pub fn encode(&self) -> String {
        let mut text = String::from("TDVM-SETTINGS-PROFILE-1\n");
        for (name, value) in &self.fields {
            text.push_str(&format!("{name}={value}\n"));
        }
        text
    }

    pub fn fingerprint(&self) -> String {
        sha256::hex_digest(self.encode().as_bytes())
    }

    pub fn summary(&self) -> Result<String> {
        Ok(format!(
            "Default agent: {}\nHost workspace: {}\nHost PATH: {}\nCodex: {}\n  executable: {} ({})\n  home: {}\n  settings: {}\nClaude: {}\n  executable: {} ({})\n  home: {}\n  settings: {}\nSettings profile: {}",
            self.get("default-agent")?,
            self.get("workspace")?,
            self.get("path")?,
            self.get("codex-version")?,
            self.get("codex")?,
            self.get("codex-sha256")?,
            self.get("codex-home")?,
            self.get("codex-config-sha256")?,
            self.get("claude-version")?,
            self.get("claude")?,
            self.get("claude-sha256")?,
            self.get("claude-home")?,
            self.get("claude-settings-sha256")?,
            self.fingerprint(),
        ))
    }

    fn version(&self, program: &Path, home: &str, home_variable: &str) -> Result<String> {
        let mut command = Command::new(program);
        command
            .env_clear()
            .current_dir("/")
            .env("HOME", self.get(home)?)
            .env(home_variable, self.get(home)?)
            .env("PATH", self.get("path")?)
            .arg("--version");
        let bytes = vm_git_profile::capture(command)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| "agent version output is not UTF-8")?;
        let value = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        if value.contains(['\n', '\r']) || value.is_empty() || value.len() > 256 {
            return Err("agent version output must be one bounded line".into());
        }
        Ok(value.into())
    }

    fn check_with_boundary(&self, boundary: Option<&Path>) -> Result<String> {
        trusted_directory(self.get("workspace")?, "host workspace", boundary)?;
        let codex_home = trusted_directory(self.get("codex-home")?, "Codex home", boundary)?;
        let claude_home = trusted_directory(self.get("claude-home")?, "Claude home", boundary)?;
        path_list(self.get("path")?, boundary)?;
        let codex_program = trusted_program(self.get("codex")?, boundary)?;
        let claude_program = trusted_program(self.get("claude")?, boundary)?;
        let codex = settings(&codex_home.join("config.toml"), "Codex config.toml")?;
        let claude = settings(&claude_home.join("settings.json"), "Claude settings.json")?;
        if sha256::hex_digest(&codex) != self.get("codex-config-sha256")? {
            return Err(
                "Codex settings changed; review them and replace the settings profile".into(),
            );
        }
        if sha256::hex_digest(&claude) != self.get("claude-settings-sha256")? {
            return Err(
                "Claude settings changed; review them and replace the settings profile".into(),
            );
        }
        if executable_fingerprint(&codex_program, "Codex executable")?
            != self.get("codex-sha256")?
        {
            return Err(
                "Codex executable changed; review it and replace the settings profile".into(),
            );
        }
        if executable_fingerprint(&claude_program, "Claude executable")?
            != self.get("claude-sha256")?
        {
            return Err(
                "Claude executable changed; review it and replace the settings profile".into(),
            );
        }
        if self.version(&codex_program, "codex-home", "CODEX_HOME")? != self.get("codex-version")? {
            return Err("Codex version changed; review it and replace the settings profile".into());
        }
        if self.version(&claude_program, "claude-home", "CLAUDE_CONFIG_DIR")?
            != self.get("claude-version")?
        {
            return Err(
                "Claude version changed; review it and replace the settings profile".into(),
            );
        }
        Ok(format!(
            "Settings source profile {} is unchanged and both configured CLIs match. Guest schema translation, synchronization, and login reuse remain pending.",
            self.fingerprint()
        ))
    }

    pub fn check(&self) -> Result<String> {
        self.check_with_boundary(None)
    }

    #[cfg(test)]
    fn check_beneath(&self, boundary: &Path) -> Result<String> {
        self.check_with_boundary(Some(boundary))
    }
}

fn read_profile(path: &Path, private: bool) -> Result<Profile> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_READ_FLAGS)
            .open(path),
        "open settings profile",
    )?;
    let metadata = io(file.metadata(), "inspect settings profile")?;
    let uid = io(fs::metadata("/proc/self"), "inspect current UID")?.uid();
    if !metadata.is_file()
        || ![0, uid].contains(&metadata.uid())
        || metadata.nlink() != 1
        || metadata.mode() & (if private { 0o077 } else { 0o022 }) != 0
    {
        return Err(
            "settings profile must be a trusted regular file with restricted permissions".into(),
        );
    }
    let mut text = String::new();
    io(
        file.take(PROFILE_LIMIT + 1).read_to_string(&mut text),
        "read settings profile",
    )?;
    Profile::parse(&text)
}

pub fn load(root: &Path) -> Result<Profile> {
    read_profile(&root.join(FILE), true)
}

pub fn configure(root: &Path, input: &Path) -> Result<()> {
    let profile = read_profile(input, false)?;
    let temporary = root.join("settings-profile.tmp");
    match fs::symlink_metadata(&temporary) {
        Ok(metadata) if metadata.is_file() => {
            io(
                fs::remove_file(&temporary),
                "remove interrupted settings profile staging",
            )?;
        }
        Ok(_) => return Err("unexpected settings profile staging entry".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect settings profile staging: {error}")),
    }
    let result = (|| {
        let mut file = io(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary),
            "stage settings profile",
        )?;
        io(
            file.write_all(profile.encode().as_bytes()),
            "write settings profile",
        )?;
        io(file.sync_all(), "sync settings profile")?;
        io(
            fs::rename(&temporary, root.join(FILE)),
            "publish settings profile",
        )?;
        io(
            File::open(root).and_then(|file| file.sync_all()),
            "sync settings profile directory",
        )
    })();
    let _ = fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn example() -> String {
        format!(
            "TDVM-SETTINGS-PROFILE-1\ndefault-agent=codex\nworkspace=/home/test/src/td\npath=/usr/bin\ncodex-home=/home/test/.codex\ncodex=/usr/bin/codex\ncodex-sha256={}\ncodex-version=codex-cli 0.148.0\ncodex-config-sha256={}\nclaude-home=/home/test/.claude\nclaude=/usr/bin/claude\nclaude-sha256={}\nclaude-version=2.1.260 (Claude Code)\nclaude-settings-sha256={}\n",
            "c".repeat(64),
            "a".repeat(64),
            "d".repeat(64),
            "b".repeat(64)
        )
    }

    #[test]
    fn profile_roundtrip_and_secret_or_ambiguous_fields_are_refused() {
        let text = example();
        let profile = Profile::parse(&text).unwrap();
        assert_eq!(Profile::parse(&profile.encode()).unwrap(), profile);
        assert_eq!(profile.fingerprint().len(), 64);
        for bad in [
            text.replace("default-agent=codex", "default-agent=other"),
            text.replace("workspace=/home/test/src/td", "workspace=../td"),
            text.replace("path=/usr/bin", "path=/usr/bin::/bin"),
            text.replace(&"a".repeat(64), "A234"),
            format!("{text}codex-auth=/home/test/.codex/auth.json\n"),
            text.trim_end().into(),
            text.replace("claude-version=2.1.260 (Claude Code)\n", ""),
        ] {
            assert!(Profile::parse(&bad).is_err(), "{bad}");
        }
        assert!(!profile.encode().contains("auth.json"));
        assert!(!profile.encode().contains("credentials.json"));
    }

    #[test]
    fn check_binds_exact_settings_versions_and_file_trust() {
        let root = Path::new("/tmp").join(format!(
            "td-vm-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for path in [
            root.clone(),
            root.join("workspace"),
            root.join("codex"),
            root.join("claude"),
            root.join("bin"),
        ] {
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        }
        let codex_config = root.join("codex/config.toml");
        let claude_settings = root.join("claude/settings.json");
        fs::write(&codex_config, b"model = \"fixture\"\n").unwrap();
        fs::write(&claude_settings, b"{}\n").unwrap();
        for name in ["codex-tool", "claude-tool"] {
            let path = root.join("bin").join(name);
            fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{name}-1'\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let text = format!(
            "TDVM-SETTINGS-PROFILE-1\ndefault-agent=codex\nworkspace={}\npath={}\ncodex-home={}\ncodex={}\ncodex-sha256={}\ncodex-version=codex-tool-1\ncodex-config-sha256={}\nclaude-home={}\nclaude={}\nclaude-sha256={}\nclaude-version=claude-tool-1\nclaude-settings-sha256={}\n",
            root.join("workspace").display(),
            root.join("bin").display(),
            root.join("codex").display(),
            root.join("bin/codex-tool").display(),
            sha256::hex_digest(b"#!/bin/sh\nprintf '%s\\n' 'codex-tool-1'\n"),
            sha256::hex_digest(b"model = \"fixture\"\n"),
            root.join("claude").display(),
            root.join("bin/claude-tool").display(),
            sha256::hex_digest(b"#!/bin/sh\nprintf '%s\\n' 'claude-tool-1'\n"),
            sha256::hex_digest(b"{}\n"),
        );
        let profile = Profile::parse(&text).unwrap();
        assert!(profile
            .check_beneath(&root)
            .unwrap()
            .contains("login reuse remain pending"));
        fs::write(&codex_config, b"model = \"changed\"\n").unwrap();
        assert!(profile
            .check_beneath(&root)
            .unwrap_err()
            .contains("settings changed"));
        fs::write(&codex_config, b"model = \"fixture\"\n").unwrap();
        let codex_tool = root.join("bin/codex-tool");
        fs::write(
            &codex_tool,
            b"#!/bin/sh\n# changed\nprintf '%s\\n' 'codex-tool-1'\n",
        )
        .unwrap();
        assert!(profile
            .check_beneath(&root)
            .unwrap_err()
            .contains("Codex executable changed"));
        fs::write(&codex_tool, b"#!/bin/sh\nprintf '%s\\n' 'codex-tool-1'\n").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(profile
            .check_beneath(&root)
            .unwrap_err()
            .contains("ancestor"));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&claude_settings, fs::Permissions::from_mode(0o622)).unwrap();
        assert!(profile
            .check_beneath(&root)
            .unwrap_err()
            .contains("trusted singly linked"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configure_loads_private_generation_and_handles_staging() {
        let root = std::env::temp_dir().join(format!(
            "td-vm-settings-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let input = root.join("input");
        fs::write(&input, example()).unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o644)).unwrap();

        configure(&root, &input).unwrap();
        assert_eq!(load(&root).unwrap(), Profile::parse(&example()).unwrap());
        assert_eq!(
            fs::symlink_metadata(root.join(FILE)).unwrap().mode() & 0o777,
            0o600
        );

        fs::write(root.join("settings-profile.tmp"), b"interrupted").unwrap();
        configure(&root, &input).unwrap();
        assert_eq!(load(&root).unwrap(), Profile::parse(&example()).unwrap());

        fs::create_dir(root.join("settings-profile.tmp")).unwrap();
        assert!(configure(&root, &input)
            .unwrap_err()
            .contains("unexpected settings profile staging entry"));
        assert_eq!(load(&root).unwrap(), Profile::parse(&example()).unwrap());
        fs::remove_dir(root.join("settings-profile.tmp")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
