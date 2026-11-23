//! The operator's deployment target: how the launcher reaches the host, and
//! what the supervisor on that host requires of itself. Neither is compiled
//! in; td-mta/PRODUCTION.md, "Deploy target configuration", owns the format.
use crate::{
    common::{child, fmt, owned, require},
    R,
};
use std::{
    ffi::OsString,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
};
use td_json::Json;

/// The supervisor's root-owned description of the host it runs on.
pub const HOST_CONFIG: &str = "/var/lib/td-mta-operator/host.json";
const LIMIT: u64 = 4096;
const LAUNCHER_KEYS: &[&str] = &[
    "host",
    "host_name",
    "account",
    "identity",
    "jmap_account",
    "jmap_url",
];
const HOST_KEYS: &[&str] = &["host_name", "build_account", "build_home", "public_name"];
/// The flags `--write-target` takes, one per `target.json` field.
pub const TARGET_FLAGS: &[(&str, &str)] = &[
    ("--host", "host"),
    ("--host-name", "host_name"),
    ("--account", "account"),
    ("--identity", "identity"),
    ("--jmap-account", "jmap_account"),
    ("--jmap-url", "jmap_url"),
];
/// The flags `--provision-host` takes; `host_name` comes from target.json.
pub const HOST_FLAGS: &[(&str, &str)] = &[
    ("--build-account", "build_account"),
    ("--build-home", "build_home"),
    ("--public-name", "public_name"),
];

/// The development machine's `target.json`.
#[derive(Debug, PartialEq)]
pub struct Target {
    /// `account@host`, the SSH destination and the push remote's prefix.
    pub destination: String,
    /// What the host's own host.json must name, checked before any change.
    pub host_name: String,
    pub identity: PathBuf,
    pub jmap_account: String,
    pub jmap_url: String,
}

/// The host's `host.json`.
#[derive(Debug, PartialEq)]
pub struct HostConfig {
    pub host_name: String,
    pub build_account: String,
    pub build_home: String,
    pub public_name: String,
}

fn fields<'a>(text: &'a Json, keys: &[&str], what: &str) -> R<&'a [(String, Json)]> {
    let object = text.as_obj().ok_or(what)?;
    require(
        object.len() == keys.len() && object.iter().all(|(k, _)| keys.contains(&k.as_str())),
        what,
    )?;
    Ok(object)
}

fn text<'a>(
    object: &'a [(String, Json)],
    key: &str,
    valid: fn(&str) -> bool,
    what: &str,
) -> R<&'a str> {
    let value = object
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_str())
        .ok_or(what)?;
    require(valid(value), what)?;
    Ok(value)
}

fn printable(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

/// A DNS name or IPv4 address: what both ssh and Git's scp-like remote
/// syntax accept unbracketed, and what may go in SNI and a Host header.
fn dns_name(value: &str) -> bool {
    value.len() <= 253
        && value.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn host_name(value: &str) -> bool {
    value.len() <= 64 && dns_name(value)
}

fn account(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

/// Absolute, with no empty, `.` or `..` component.
fn absolute(value: &str) -> bool {
    printable(value)
        && value.strip_prefix('/').is_some_and(|rest| {
            rest.split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..")
        })
}

/// Absolute, or `~/` and a relative path below the home directory, with
/// none of the `%` and `$` expansions `ssh -i` performs.
fn identity(value: &str) -> bool {
    absolute(value.strip_prefix('~').unwrap_or(value)) && !value.contains(['%', '$'])
}

/// A build home also goes into a `PATH` list.
fn build_home(value: &str) -> bool {
    absolute(value) && !value.contains(':')
}

/// A certificate's DNS name: dotted, and not an IPv4 literal.
fn public_name(value: &str) -> bool {
    dns_name(value)
        && value.contains('.')
        && value
            .rsplit('.')
            .next()
            .is_some_and(|tld| !tld.bytes().all(|b| b.is_ascii_digit()))
}

fn url(value: &str) -> bool {
    printable(value)
        && !value.contains(char::is_whitespace)
        && value
            .strip_prefix("https://")
            .and_then(|rest| rest.split('/').next())
            .is_some_and(dns_name)
}

/// One JSON object with `values` under `keys`, in that order: the text the
/// two commands that create these files write.
fn render(keys: &[&str], values: &[(&str, &str)]) -> R<String> {
    let mut object = Vec::new();
    for key in keys {
        let mut found = values.iter().filter(|(k, _)| k == key);
        let (Some((_, value)), None) = (found.next(), found.next()) else {
            return Err("every configuration field needs exactly one value".into());
        };
        td_alloc::push(&mut object, (owned(key)?, Json::Str(owned(value)?)))?;
    }
    require(values.len() == keys.len(), "unexpected configuration field")?;
    fmt(format_args!("{}\n", Json::Obj(object)))
}

/// The file `Target::load` reads: `path`, else `target.json` under
/// `$XDG_CONFIG_HOME/td-mta-deploy` when that is absolute, else under
/// `~/.config/td-mta-deploy`.
pub fn target_path(path: Option<&Path>, home: &Path, xdg: Option<&Path>) -> R<PathBuf> {
    match path {
        Some(path) => owned_path(path),
        None => {
            let base = match xdg {
                Some(base) if base.is_absolute() => owned_path(base)?,
                _ => child(home, ".config")?,
            };
            child(&base, "td-mta-deploy/target.json")
        }
    }
}

impl Target {
    /// `target.json`'s text for `values`, keyed by field, and what it says.
    pub fn render(values: &[(&str, &str)], home: &Path) -> R<(String, Self)> {
        let text = render(LAUNCHER_KEYS, values)?;
        let target = Self::parse(text.as_bytes(), home)?;
        Ok((text, target))
    }

    /// The administrator account, the login part of `destination`.
    pub fn account(&self) -> Option<&str> {
        self.destination.split_once('@').map(|(account, _)| account)
    }

    /// Read the file `target_path` names. `~/` in `identity` names `home`.
    pub fn load(path: Option<&Path>, home: &Path, xdg: Option<&Path>) -> R<Self> {
        let path = target_path(path, home, xdg)?;
        let bytes = td_fs::read_bounded(&path, LIMIT).map_err(|_| {
            "cannot read the deploy target configuration: write it with td-mta/deploy --write-target (td-mta/PRODUCTION.md, \"Deploy target configuration\")"
        })?;
        Self::parse(&bytes, home)
    }

    pub fn parse(bytes: &[u8], home: &Path) -> R<Self> {
        require(
            bytes.len() as u64 <= LIMIT,
            "deploy target configuration too large",
        )?;
        let json = td_json::parse_slice(bytes)
            .map_err(|_| "deploy target configuration is not a JSON object")?;
        let object = fields(&json, LAUNCHER_KEYS, "deploy target configuration needs exactly host, host_name, account, identity, jmap_account and jmap_url")?;
        let host = text(object, "host", dns_name, "deploy target: invalid host")?;
        let host_name = owned(text(
            object,
            "host_name",
            host_name,
            "deploy target: invalid host_name",
        )?)?;
        let account = text(object, "account", account, "deploy target: invalid account")?;
        let key = text(
            object,
            "identity",
            identity,
            "deploy target: invalid identity",
        )?;
        let identity = match key.strip_prefix("~/") {
            Some(rest) => child(home, rest)?,
            None => owned_path(Path::new(key))?,
        };
        let mut destination = owned(account)?;
        td_alloc::push_str(&mut destination, "@")?;
        td_alloc::push_str(&mut destination, host)?;
        Ok(Self {
            destination,
            host_name,
            identity,
            jmap_account: owned(text(
                object,
                "jmap_account",
                printable,
                "deploy target: invalid jmap_account",
            )?)?,
            jmap_url: owned(text(
                object,
                "jmap_url",
                url,
                "deploy target: invalid jmap_url",
            )?)?,
        })
    }
}

impl HostConfig {
    /// `host.json`'s text for `values`, keyed by field, and what it says.
    pub fn render(values: &[(&str, &str)]) -> R<(String, Self)> {
        let text = render(HOST_KEYS, values)?;
        let config = Self::parse(text.as_bytes())?;
        Ok((text, config))
    }

    /// The supervisor's own read: a root-owned, mode-0600 regular file.
    pub fn load() -> R<Self> {
        Self::load_file(Path::new(HOST_CONFIG), 0)
    }

    fn load_file(path: &Path, owner: u32) -> R<Self> {
        let unreadable = "cannot read /var/lib/td-mta-operator/host.json: provision it with td-mta/deploy --provision-host";
        let (file, meta) =
            td_fs::open_real_file(path, "host configuration").map_err(|_| unreadable)?;
        require(
            meta.uid() == owner && meta.mode() & 0o7777 == 0o600 && meta.len() <= LIMIT,
            "/var/lib/td-mta-operator/host.json must be root's, mode 0600, at most 4096 bytes",
        )?;
        let mut bytes = td_alloc::filled(usize::try_from(meta.len())?, 0)?;
        std::io::Read::read_exact(&mut &file, &mut bytes).map_err(|_| unreadable)?;
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> R<Self> {
        require(bytes.len() as u64 <= LIMIT, "host configuration too large")?;
        let json =
            td_json::parse_slice(bytes).map_err(|_| "host configuration is not a JSON object")?;
        let object = fields(
            &json,
            HOST_KEYS,
            "host configuration needs exactly host_name, build_account, build_home and public_name",
        )?;
        Ok(Self {
            host_name: owned(text(
                object,
                "host_name",
                host_name,
                "host configuration: invalid host_name",
            )?)?,
            build_account: owned(text(
                object,
                "build_account",
                account,
                "host configuration: invalid build_account",
            )?)?,
            build_home: owned(text(
                object,
                "build_home",
                build_home,
                "host configuration: invalid build_home",
            )?)?,
            public_name: owned(text(
                object,
                "public_name",
                public_name,
                "host configuration: invalid public_name",
            )?)?,
        })
    }

    /// The build account's source mirror, into which the launcher pushes.
    pub fn mirror(&self) -> R<PathBuf> {
        child(Path::new(&self.build_home), "src/td")
    }
}

fn owned_path(path: &Path) -> R<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(td_alloc::copy(
        path.as_os_str().as_bytes(),
    )?)))
}

#[cfg(test)]
#[path = "tests/target.rs"]
mod tests;
