use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const CONFIG_PATH: &str = "/etc/td-app.conf";
const REGISTRY_PATH: &str = "/etc/td-applications.tsv";
const PACKAGE_ROOT: &str = "/td/store";
const STATE_ROOT: &str = ".td/app";
pub(crate) const RUNTIME_ROOT_NAME: &str = "td-app";
const CONFIG: &str = "format=1\npackage-root=/td/store\nstate-root=.td/app\nregistry=/etc/td-applications.tsv\nlauncher-table=/etc/td-launcher.tsv\n";
const SPEC_FORMAT: &str = "format=1";
const NAME_PREFIX: &str = "name=";
const RUNTIME_PREFIX: &str = "runtime=";
const ENTRY_PREFIX: &str = "entry=";
const ENVIRONMENT_SECTION: &str = "[Environment]";
const CONTEXT_SECTION: &str = "[Context]";
const WAYLAND_POLICY: &str = "sockets=wayland";
const MAX_CONFIG_BYTES: usize = 4096;
const MAX_STATUS_BYTES: usize = 64 * 1024;
const MAX_PASSWD_BYTES: usize = 64 * 1024;
const MAX_APPLICATION_SPEC_BYTES: usize = 48 * 1024;
const MAX_APPLICATION_TABLE_BYTES: usize = 1024 * 1024;
const MAX_APPLICATIONS: usize = 256;
const MAX_APPLICATION_NAME_BYTES: usize = 32;
pub(crate) const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 128;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4096;
const MAX_ENTRY_BYTES: usize = 4096;
const MAX_APPLICATION_ARGUMENTS: usize = 256;
const MAX_APPLICATION_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct LaunchPlan {
    pub(crate) package_files: PathBuf,
    pub(crate) runtime_files: PathBuf,
    pub(crate) state: StatePlan,
    pub(crate) wayland_socket: PathBuf,
    pub(crate) entry: String,
    pub(crate) environment: Vec<(OsString, OsString)>,
    pub(crate) arguments: Vec<OsString>,
}

#[derive(Debug)]
pub(crate) struct StatePlan {
    pub(crate) home: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) local_state: PathBuf,
    pub(crate) runtime: PathBuf,
}

pub(crate) fn application_name(argv0: &OsStr) -> io::Result<&str> {
    if argv0.as_bytes().last() == Some(&b'/') {
        return Err(invalid("argv[0] has a trailing slash"));
    }
    let name = Path::new(argv0)
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| invalid("argv[0] has no UTF-8 basename"))?;
    validate_application_name(name)?;
    Ok(name)
}

pub(crate) fn resolve<I>(name: &str, arguments: I) -> io::Result<LaunchPlan>
where
    I: Iterator<Item = OsString>,
{
    validate_application_name(name)?;
    let identity = effective_identity()?;
    if identity.0 == 0 || identity.1 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "td-jail requires the nonzero application identity",
        ));
    }
    let arguments = collect_arguments(arguments)?;
    let config = read_bounded(CONFIG_PATH, MAX_CONFIG_BYTES)?;
    if config != CONFIG {
        return Err(invalid(
            "application configuration is not the compiled product configuration",
        ));
    }

    let registry_text = read_bounded(REGISTRY_PATH, MAX_APPLICATION_TABLE_BYTES)?;
    let package = registry_entry(&registry_text, name)?
        .ok_or_else(|| invalid(format!("application {name:?} is not installed")))?;
    let package = canonical_store_directory(&package, "application package")?;
    // Installation authenticated the manifest; launch pins its immutable
    // package-layout slot without interpreting recipe metadata again.
    require_regular(&package.join("manifest"), "application manifest")?;

    let package_files =
        canonical_child_directory(&package, "files", "application files")?;
    let spec_path = package.join("spec");
    require_regular(&spec_path, "application spec")?;
    let spec_text = read_bounded_path(&spec_path, MAX_APPLICATION_SPEC_BYTES)?;
    let spec = parse_spec(&spec_text)?;
    if spec.name != name {
        return Err(invalid(format!(
            "application spec names {:?}, but argv[0] selected {name:?}",
            spec.name
        )));
    }

    let runtime = PathBuf::from(&spec.runtime);
    let runtime = canonical_store_directory(&runtime, "application runtime")?;
    let runtime_files =
        canonical_child_directory(&runtime, "files", "application runtime files")?;

    let relative_entry = spec
        .entry
        .strip_prefix("/app/")
        .ok_or_else(|| invalid("application entry is outside /app"))?;
    let host_entry = package_files.join(relative_entry);
    let entry_metadata = fs::symlink_metadata(&host_entry).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "inspect application entry {}: {error}",
                host_entry.display()
            ),
        )
    })?;
    if !entry_metadata.file_type().is_file() || entry_metadata.mode() & 0o111 == 0 {
        return Err(invalid(format!(
            "application entry {} is not a directly executable regular file",
            host_entry.display()
        )));
    }

    let environment = spec
        .environment
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<Vec<_>>();
    validate_environment_list(&environment, identity.0)?;
    let wayland_socket = PathBuf::from(format!("/run/user/{}/wayland-0", identity.0));
    require_wayland_socket(&wayland_socket, identity.0)?;
    let wayland_socket = fs::canonicalize(&wayland_socket)?;
    require_wayland_socket(&wayland_socket, identity.0)?;
    let state = prepare_state(name, identity)?;

    Ok(LaunchPlan {
        package_files,
        runtime_files,
        state,
        wayland_socket,
        entry: spec.entry,
        environment,
        arguments,
    })
}

pub(crate) fn collect_arguments<I>(arguments: I) -> io::Result<Vec<OsString>>
where
    I: Iterator<Item = OsString>,
{
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for argument in arguments {
        if out.len() >= MAX_APPLICATION_ARGUMENTS {
            return Err(invalid(format!(
                "application invocation exceeds {MAX_APPLICATION_ARGUMENTS} arguments"
            )));
        }
        let raw = argument.as_bytes();
        if raw.contains(&0) {
            return Err(invalid("application argument contains NUL"));
        }
        bytes = bytes
            .checked_add(raw.len())
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| invalid("application argument size overflow"))?;
        if bytes > MAX_APPLICATION_ARGUMENT_BYTES {
            return Err(invalid(format!(
                "application arguments exceed {MAX_APPLICATION_ARGUMENT_BYTES} bytes"
            )));
        }
        out.push(argument);
    }
    Ok(out)
}

pub(crate) fn validate_environment_list(
    environment: &[(OsString, OsString)],
    uid: u32,
) -> io::Result<()> {
    if environment.len() > MAX_ENVIRONMENT_ENTRIES {
        return Err(invalid(format!(
            "application environment exceeds {MAX_ENVIRONMENT_ENTRIES} entries"
        )));
    }
    let mut bytes = 0usize;
    let mut previous = None;
    for (key, value) in environment {
        let key = key
            .to_str()
            .ok_or_else(|| invalid("application environment name is not UTF-8"))?;
        let value = value
            .to_str()
            .ok_or_else(|| invalid("application environment value is not UTF-8"))?;
        validate_environment_entry(key, value)?;
        if previous.is_some_and(|prior: &str| prior >= key) {
            return Err(invalid("application environment is not strictly sorted"));
        }
        bytes = bytes
            .checked_add(key.len())
            .and_then(|size| size.checked_add(value.len()))
            .and_then(|size| size.checked_add(2))
            .ok_or_else(|| invalid("application environment size overflow"))?;
        if bytes > MAX_APPLICATION_SPEC_BYTES {
            return Err(invalid(format!(
                "application environment exceeds {MAX_APPLICATION_SPEC_BYTES} bytes"
            )));
        }
        previous = Some(key);
    }

    for (key, expected) in [
        ("HOME", "/home/td".to_string()),
        ("WAYLAND_DISPLAY", "wayland-0".to_string()),
        ("XDG_RUNTIME_DIR", format!("/run/user/{uid}")),
    ] {
        let mut actual = None;
        for (candidate, value) in environment {
            if candidate == key {
                actual = Some(value.to_string_lossy().into_owned());
                break;
            }
        }
        if actual.as_deref() != Some(expected.as_str()) {
            return Err(invalid(format!(
                "application environment {key} is {actual:?}, expected {expected:?}"
            )));
        }
    }
    if !environment
        .iter()
        .any(|(key, value)| key == "FLATPAK_ID" && !value.is_empty())
    {
        return Err(invalid("application environment lacks FLATPAK_ID"));
    }
    Ok(())
}

struct ParsedSpec {
    name: String,
    runtime: String,
    entry: String,
    environment: BTreeMap<String, String>,
}

fn parse_spec(text: &str) -> io::Result<ParsedSpec> {
    validate_text("application spec", text, MAX_APPLICATION_SPEC_BYTES)?;
    let mut lines = text.lines();
    require_line(&mut lines, SPEC_FORMAT, "application spec format")?;
    let name = prefixed_line(&mut lines, NAME_PREFIX, "application name")?;
    validate_application_name(&name)?;
    let runtime = prefixed_line(&mut lines, RUNTIME_PREFIX, "application runtime")?;
    require_store_child(Path::new(&runtime), "application runtime")?;
    let entry = prefixed_line(&mut lines, ENTRY_PREFIX, "application entry")?;
    validate_entry(&entry)?;
    require_line(&mut lines, "", "application spec section separator")?;
    require_line(
        &mut lines,
        ENVIRONMENT_SECTION,
        "application environment section",
    )?;

    let mut environment = BTreeMap::new();
    let mut previous = None;
    loop {
        let line = lines
            .next()
            .ok_or_else(|| invalid("application spec lacks its Wayland policy"))?;
        if line.is_empty() {
            break;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| invalid("application environment row has no `='"))?;
        validate_environment_entry(key, value)?;
        if previous.as_deref().is_some_and(|prior| prior >= key) {
            return Err(invalid("application environment is not strictly sorted"));
        }
        if environment.len() >= MAX_ENVIRONMENT_ENTRIES {
            return Err(invalid(format!(
                "application environment exceeds {MAX_ENVIRONMENT_ENTRIES} entries"
            )));
        }
        previous = Some(key.to_string());
        environment.insert(key.to_string(), value.to_string());
    }
    require_line(&mut lines, CONTEXT_SECTION, "application context section")?;
    require_line(&mut lines, WAYLAND_POLICY, "application Wayland policy")?;
    if lines.next().is_some() {
        return Err(invalid(
            "application requests policy not implemented by this td-jail rung",
        ));
    }
    Ok(ParsedSpec {
        name,
        runtime,
        entry,
        environment,
    })
}

fn registry_entry(text: &str, selected: &str) -> io::Result<Option<PathBuf>> {
    validate_text("application registry", text, MAX_APPLICATION_TABLE_BYTES)?;
    let mut found = None;
    let mut previous = None;
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_APPLICATIONS {
            return Err(invalid(format!(
                "application registry exceeds {MAX_APPLICATIONS} entries"
            )));
        }
        let mut fields = line.split('\t');
        let name = fields
            .next()
            .ok_or_else(|| invalid("application registry row has no name"))?;
        let path = fields
            .next()
            .ok_or_else(|| invalid("application registry row has no path"))?;
        if fields.next().is_some() {
            return Err(invalid("application registry row has extra fields"));
        }
        validate_application_name(name)?;
        if previous.is_some_and(|prior: &str| prior >= name) {
            return Err(invalid("application registry is not strictly sorted"));
        }
        require_store_child(Path::new(path), "application package")?;
        if name == selected {
            found = Some(PathBuf::from(path));
        }
        previous = Some(name);
    }
    Ok(found)
}

fn validate_application_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > MAX_APPLICATION_NAME_BYTES
        || name.starts_with('-')
        || name == "."
        || name.contains("..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid(
            "application name is outside the compiled name grammar",
        ));
    }
    Ok(())
}

pub(crate) fn validate_entry(entry: &str) -> io::Result<()> {
    let relative = entry
        .strip_prefix("/app/")
        .ok_or_else(|| invalid("application entry is outside /app"))?;
    if relative.is_empty()
        || relative.len() > MAX_ENTRY_BYTES
        || entry.trim() != entry
        || entry.chars().any(char::is_control)
        || relative
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid("application entry is not a canonical /app path"));
    }
    Ok(())
}

fn validate_environment_entry(key: &str, value: &str) -> io::Result<()> {
    let mut bytes = key.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if !valid_first
        || key.len() > MAX_ENVIRONMENT_NAME_BYTES
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || key.starts_with("TD_")
        || key.starts_with("LD_")
        || value.len() > MAX_ENVIRONMENT_VALUE_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid(
            "application environment is outside the compiled grammar",
        ));
    }
    Ok(())
}

fn require_line<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    expected: &str,
    label: &str,
) -> io::Result<()> {
    let actual = lines
        .next()
        .ok_or_else(|| invalid(format!("{label} is missing")))?;
    if actual != expected {
        return Err(invalid(format!(
            "{label} is {actual:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn prefixed_line<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    prefix: &str,
    label: &str,
) -> io::Result<String> {
    lines
        .next()
        .and_then(|line| line.strip_prefix(prefix))
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("{label} is missing or out of order")))
}

fn validate_text(label: &str, text: &str, limit: usize) -> io::Result<()> {
    if text.is_empty()
        || text.len() > limit
        || !text.ends_with('\n')
        || text.contains('\r')
        || text.contains('\0')
    {
        return Err(invalid(format!("{label} has invalid bounded text shape")));
    }
    Ok(())
}

fn require_store_child(path: &Path, label: &str) -> io::Result<()> {
    let raw = path.as_os_str().as_bytes();
    let name = raw
        .strip_prefix(PACKAGE_ROOT.as_bytes())
        .and_then(|suffix| suffix.strip_prefix(b"/"))
        .unwrap_or_default();
    if raw.len() > 4096
        || name.is_empty()
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.iter().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid(format!(
            "{label} {} is not one canonical /td/store child",
            path.display()
        )));
    }
    Ok(())
}

fn require_regular(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("inspect {label} {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!(
            "{label} {} is not a regular file",
            path.display()
        )));
    }
    Ok(())
}

fn require_directory(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("inspect {label} {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(format!(
            "{label} {} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

fn canonical_directory(path: &Path, label: &str) -> io::Result<PathBuf> {
    require_directory(path, label)?;
    let canonical = fs::canonicalize(path)?;
    require_directory(&canonical, label)?;
    Ok(canonical)
}

fn canonical_store_directory(path: &Path, label: &str) -> io::Result<PathBuf> {
    require_store_child(path, label)?;
    let canonical = canonical_directory(path, label)?;
    require_store_child(&canonical, label)?;
    Ok(canonical)
}

fn canonical_child_directory(parent: &Path, name: &str, label: &str) -> io::Result<PathBuf> {
    let expected = parent.join(name);
    let canonical = canonical_directory(&expected, label)?;
    if canonical != expected {
        return Err(invalid(format!(
            "{label} {} resolves outside its immutable store object",
            expected.display()
        )));
    }
    Ok(canonical)
}

fn require_wayland_socket(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(invalid(format!(
            "Wayland authority {} is not a socket owned by uid {uid}",
            path.display()
        )));
    }
    Ok(())
}

fn effective_identity() -> io::Result<(u32, u32)> {
    let status = read_bounded("/proc/self/status", MAX_STATUS_BYTES)?;
    Ok((status_id(&status, "Uid:")?, status_id(&status, "Gid:")?))
}

fn status_id(status: &str, key: &str) -> io::Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|fields| fields.split_whitespace().nth(1))
        .ok_or_else(|| invalid(format!("/proc/self/status lacks an effective {key} field")))?
        .parse()
        .map_err(|error| invalid(format!("invalid /proc/self/status {key} field: {error}")))
}

fn prepare_state(name: &str, identity: (u32, u32)) -> io::Result<StatePlan> {
    let passwd = read_bounded("/etc/passwd", MAX_PASSWD_BYTES)?;
    let mut home = None;
    for line in passwd.lines() {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() != 7 {
            return Err(invalid("/etc/passwd contains a noncanonical row"));
        }
        let Some(uid) = fields.get(2).and_then(|value| value.parse::<u32>().ok()) else {
            return Err(invalid("/etc/passwd contains an invalid uid"));
        };
        if uid != identity.0 {
            continue;
        }
        if home.is_some() {
            return Err(invalid(
                "/etc/passwd maps the application uid more than once",
            ));
        }
        let value = fields
            .get(5)
            .ok_or_else(|| invalid("/etc/passwd row has no home directory"))?;
        home = Some(PathBuf::from(value));
    }
    let home = home.ok_or_else(|| invalid("application uid has no /etc/passwd entry"))?;
    require_owned_directory(&home, identity, false)?;
    let home = fs::canonicalize(&home)?;
    require_owned_directory(&home, identity, false)?;
    let (state_parent, state_applications) = STATE_ROOT
        .split_once('/')
        .ok_or_else(|| invalid("compiled state root is not canonical"))?;
    let td = ensure_state_component(&home, state_parent, identity, false)?;
    let applications = ensure_state_component(&td, state_applications, identity, true)?;
    let application = ensure_state_component(&applications, name, identity, true)?;
    let private_home = ensure_state_component(&application, "home", identity, true)?;
    // These empty children are bind targets; persistent contents live in the
    // sibling config, cache, data, and state trees.
    ensure_state_component(&private_home, ".config", identity, true)?;
    ensure_state_component(&private_home, ".cache", identity, true)?;
    let local = ensure_state_component(&private_home, ".local", identity, true)?;
    ensure_state_component(&local, "share", identity, true)?;
    ensure_state_component(&local, "state", identity, true)?;
    let config = ensure_state_component(&application, "config", identity, true)?;
    let cache = ensure_state_component(&application, "cache", identity, true)?;
    let data = ensure_state_component(&application, "data", identity, true)?;
    let local_state = ensure_state_component(&application, "state", identity, true)?;
    let runtime_root = PathBuf::from(format!("/run/user/{}", identity.0));
    require_owned_directory(&runtime_root, identity, true)?;
    let runtime_applications =
        ensure_state_component(&runtime_root, RUNTIME_ROOT_NAME, identity, true)?;
    let runtime = ensure_state_component(&runtime_applications, name, identity, true)?;
    Ok(StatePlan {
        home: private_home,
        config,
        cache,
        data,
        local_state,
        runtime,
    })
}

fn ensure_state_component(
    parent: &Path,
    name: &str,
    identity: (u32, u32),
    private: bool,
) -> io::Result<PathBuf> {
    let path = parent.join(name);
    let mut initialized = false;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => initialized = true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let before = require_owned_directory_identity(&path, identity)?;
    if private || initialized {
        let directory = OpenOptions::new().read(true).open(&path)?;
        let opened = directory.metadata()?;
        if !opened.file_type().is_dir()
            || opened.uid() != identity.0
            || opened.gid() != identity.1
            || opened.dev() != before.dev()
            || opened.ino() != before.ino()
        {
            return Err(invalid(format!(
                "state directory {} changed during validation",
                path.display()
            )));
        }
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    require_owned_directory(&path, identity, private)?;
    fs::canonicalize(path)
}

fn require_owned_directory_identity(
    path: &Path,
    identity: (u32, u32),
) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != identity.0
        || metadata.gid() != identity.1
    {
        return Err(invalid(format!(
            "state directory {} is not application-owned",
            path.display()
        )));
    }
    Ok(metadata)
}

fn require_owned_directory(path: &Path, identity: (u32, u32), private: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != identity.0
        || metadata.gid() != identity.1
        || metadata.mode() & 0o022 != 0
        || (private && metadata.mode() & 0o7777 != 0o700)
    {
        return Err(invalid(format!(
            "state directory {} is not a private application-owned directory",
            path.display()
        )));
    }
    Ok(())
}

fn read_bounded(path: &str, limit: usize) -> io::Result<String> {
    read_bounded_path(Path::new(path), limit)
}

fn read_bounded_path(path: &Path, limit: usize) -> io::Result<String> {
    let file = OpenOptions::new().read(true).open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("open {}: {error}", path.display()),
        )
    })?;
    require_open_regular(&file, path)?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid(format!(
            "{} exceeds the {limit}-byte limit",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| invalid(format!("{} is not UTF-8: {error}", path.display())))
}

fn require_open_regular(file: &File, path: &Path) -> io::Result<()> {
    if !file.metadata()?.file_type().is_file() {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
pub(crate) fn test_parse_spec(text: &str) -> io::Result<()> {
    parse_spec(text).map(|_| ())
}

#[cfg(test)]
pub(crate) fn test_registry_entry(text: &str, selected: &str) -> io::Result<Option<PathBuf>> {
    registry_entry(text, selected)
}

#[cfg(test)]
pub(crate) fn test_limits() -> [usize; 8] {
    [
        MAX_APPLICATION_SPEC_BYTES,
        MAX_APPLICATION_TABLE_BYTES,
        MAX_APPLICATIONS,
        MAX_APPLICATION_NAME_BYTES,
        MAX_ENVIRONMENT_ENTRIES,
        MAX_ENVIRONMENT_NAME_BYTES,
        MAX_ENVIRONMENT_VALUE_BYTES,
        MAX_ENTRY_BYTES,
    ]
}

#[cfg(test)]
pub(crate) fn test_validate_application_name(name: &str) -> io::Result<()> {
    validate_application_name(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn argv0_names_are_closed_and_path_independent() {
        assert!(test_validate_application_name("td-jail-fixture").is_ok());
        assert_eq!(
            application_name(OsStr::new("/bin/td-jail-fixture")).unwrap(),
            "td-jail-fixture"
        );
        for value in ["/bin/-app", "/bin/a..b", "/bin/name/", "/bin/💣"] {
            assert!(
                application_name(OsStr::new(value)).is_err(),
                "accepted {value:?}"
            );
        }
    }

    #[test]
    fn arguments_are_bounded_without_requiring_utf8() {
        assert_eq!(
            collect_arguments([OsString::from("--flag")].into_iter())
                .unwrap()
                .len(),
            1
        );
        let many = (0..=MAX_APPLICATION_ARGUMENTS).map(|_| OsString::from("x"));
        assert!(collect_arguments(many).is_err());
        assert!(collect_arguments(
            [OsString::from("x".repeat(MAX_APPLICATION_ARGUMENT_BYTES))].into_iter()
        )
        .is_err());
    }

    #[test]
    fn product_configuration_is_exact() {
        assert!(CONFIG.ends_with("launcher-table=/etc/td-launcher.tsv\n"));
        assert_eq!(PACKAGE_ROOT, "/td/store");
        assert_eq!(REGISTRY_PATH, "/etc/td-applications.tsv");
        assert_eq!(RUNTIME_ROOT_NAME, "td-app");
        assert_eq!(test_limits().len(), 8);
    }

    #[test]
    fn bounded_read_errors_name_the_path() {
        let path = std::env::temp_dir().join(format!(
            "td-jail-missing-authority-file-{}",
            std::process::id()
        ));
        let error = read_bounded_path(&path, 1).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn existing_private_state_mode_is_repaired_without_following_symlinks() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("td-jail-authority-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let metadata = fs::symlink_metadata(&root).unwrap();
        let identity = (metadata.uid(), metadata.gid());
        let application = root.join("application");
        fs::create_dir(&application).unwrap();
        fs::set_permissions(&application, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            ensure_state_component(&root, "application", identity, true).unwrap(),
            application
        );
        assert_eq!(
            fs::symlink_metadata(&application).unwrap().mode() & 0o7777,
            0o700
        );

        let initialized = ensure_state_component(&root, "initialized", identity, false).unwrap();
        assert_eq!(
            fs::symlink_metadata(&initialized).unwrap().mode() & 0o7777,
            0o700
        );
        let unsafe_existing = root.join("unsafe-existing");
        fs::create_dir(&unsafe_existing).unwrap();
        fs::set_permissions(&unsafe_existing, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(ensure_state_component(&root, "unsafe-existing", identity, false).is_err());
        assert_eq!(
            fs::symlink_metadata(&unsafe_existing).unwrap().mode() & 0o7777,
            0o775
        );

        symlink(&application, root.join("alias")).unwrap();
        assert!(ensure_state_component(&root, "alias", identity, true).is_err());

        let physical_parent = root.join("physical-parent");
        fs::create_dir(&physical_parent).unwrap();
        let parent_alias = root.join("parent-alias");
        symlink("physical-parent", &parent_alias).unwrap();
        assert_eq!(
            ensure_state_component(&parent_alias, "state", identity, true).unwrap(),
            physical_parent.join("state")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn spec_parser_accepts_only_the_closed_canonical_subset() {
        let text = "format=1\nname=fixture\nruntime=/td/store/0123456789abcdefghijklmnopqrstuv-empty-runtime-1\nentry=/app/bin/fixture\n\n[Environment]\nHOME=/home/td\nWAYLAND_DISPLAY=wayland-0\nXDG_RUNTIME_DIR=/run/user/1000\n\n[Context]\nsockets=wayland\n";
        assert!(test_parse_spec(text).is_ok());
        let spec = parse_spec(text).unwrap();
        assert_eq!(spec.name, "fixture");
        assert_eq!(spec.entry, "/app/bin/fixture");
        assert_eq!(spec.environment.len(), 3);

        for invalid in [
            text.replace(
                "HOME=/home/td\nWAYLAND_DISPLAY=wayland-0",
                "WAYLAND_DISPLAY=wayland-0\nHOME=/home/td",
            ),
            text.replace("HOME=/home/td", "HOME= /home/td"),
            text.replace(
                "sockets=wayland\n",
                "sockets=wayland\n\n[Resources]\npids-max=4\n",
            ),
            text.replace("/app/bin/fixture", "/app/bin/../fixture"),
        ] {
            assert!(parse_spec(&invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn registry_parser_is_sorted_canonical_and_selective() {
        let text = "alpha\t/td/store/0123456789abcdefghijklmnopqrstuv-alpha-1\nfixture\t/td/store/0123456789abcdefghijklmnopqrstuv-fixture-1\n";
        assert_eq!(
            registry_entry(text, "fixture").unwrap(),
            Some(PathBuf::from(
                "/td/store/0123456789abcdefghijklmnopqrstuv-fixture-1"
            ))
        );
        assert_eq!(test_registry_entry(text, "missing").unwrap(), None);
        assert_eq!(registry_entry(text, "missing").unwrap(), None);
        assert!(registry_entry(&text.replace("alpha\t", "zeta\t"), "fixture").is_err());
        assert!(registry_entry(
            "fixture\t/td/store//0123456789abcdefghijklmnopqrstuv-fixture-1\n",
            "fixture"
        )
        .is_err());
    }

    #[test]
    fn required_environment_is_exact_for_the_effective_uid() {
        let environment = [
            (
                OsString::from("FLATPAK_ID"),
                OsString::from("org.td.Fixture"),
            ),
            (OsString::from("HOME"), OsString::from("/home/td")),
            (
                OsString::from("WAYLAND_DISPLAY"),
                OsString::from("wayland-0"),
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                OsString::from("/run/user/1000"),
            ),
        ];
        assert!(validate_environment_list(&environment, 1000).is_ok());
        assert!(validate_environment_list(&environment, 1001).is_err());
        assert!(validate_environment_list(&environment[1..], 1000).is_err());

        let unsorted = [
            (
                OsString::from("WAYLAND_DISPLAY"),
                OsString::from("wayland-0"),
            ),
            (OsString::from("HOME"), OsString::from("/home/td")),
        ];
        assert!(validate_environment_list(&unsorted, 1000).is_err());
    }
}
