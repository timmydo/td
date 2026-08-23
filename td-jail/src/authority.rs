use crate::permissions::{
    FilesystemAccess, PermissionPolicy, MAX_FILESYSTEM_ENTRIES, RESERVED_FILESYSTEM_TREES,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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
    pub(crate) bus_socket: PathBuf,
    pub(crate) filesystems: Vec<FilesystemGrant>,
    pub(crate) entry: String,
    pub(crate) environment: Vec<(OsString, OsString)>,
    pub(crate) arguments: Vec<OsString>,
}

#[derive(Debug)]
pub(crate) struct StatePlan {
    pub(crate) real_home: PathBuf,
    pub(crate) state_root: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) local_state: PathBuf,
    pub(crate) runtime: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilesystemSourceKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FilesystemGrant {
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) read_only: bool,
    pub(crate) source_kind: FilesystemSourceKind,
    pub(crate) source_device: u64,
    pub(crate) source_inode: u64,
}

pub(crate) fn validate_filesystem_target(path: &Path) -> io::Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("filesystem grant target is not UTF-8"))?;
    if text.len() > 4096 || !path.is_absolute() || text == "/" {
        return Err(invalid(
            "filesystem grant target is not a bounded absolute path",
        ));
    }
    let relative = text
        .strip_prefix('/')
        .ok_or_else(|| invalid("filesystem grant target is not absolute"))?;
    if relative
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid("filesystem grant target is not canonical"));
    }
    Ok(())
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
    let wayland_socket = session_socket("wayland-0", "Wayland authority", identity.0)?;
    // Unconditional, like the mount it feeds: APPLICATIONS.md §C's mount plan,
    // step 12, binds the bus ALWAYS, because the BROKER is the policy. A jail
    // that omitted it when a manifest asked for no bus would put that decision
    // in the component with no policy language and take it away from the one
    // that will have it.
    //
    // Today's broker does NOT have it: no per-caller filter, no match rules, an
    // admission quota keyed on a pid a jailed caller can fork past. So the
    // isolation this mount declines to enforce is not being enforced anywhere,
    // and what bounds that is the image shipping ONE application — asserted in
    // the system recipe, not promised here. APPLICATIONS.md §D carries what has
    // to land before a second one does.
    let bus_socket = session_socket("bus", "session bus", identity.0)?;
    let state = prepare_state(name, identity)?;
    let filesystems = resolve_filesystem_grants(
        &spec.permissions,
        &state,
        &package_files,
        &runtime_files,
        &wayland_socket,
        &bus_socket,
    )?;

    Ok(LaunchPlan {
        package_files,
        runtime_files,
        state,
        wayland_socket,
        bus_socket,
        filesystems,
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
        // The engine has compiled this into every spec since the application
        // tier existed; nothing enforced it, so a spec could name a bus this
        // jail does not mount, or mount one the app is told nothing about.
        // Both halves are now the same fact checked in one place.
        (
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        ),
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
    permissions: PermissionPolicy,
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
    let mut permission_text = String::from("format=1\n\n");
    let mut first = true;
    for line in lines {
        if !first {
            permission_text.push('\n');
        }
        first = false;
        permission_text.push_str(line);
    }
    permission_text.push('\n');
    let permissions = PermissionPolicy::parse(&permission_text)
        .map_err(|error| invalid(format!("application spec permissions: {error}")))?;
    if permissions.to_keyfile() != permission_text {
        return Err(invalid("application spec permissions are not canonical"));
    }
    if !permissions.is_wayland_filesystem_only() {
        return Err(invalid(
            "application requests policy not implemented by this td-jail rung",
        ));
    }
    Ok(ParsedSpec {
        name,
        runtime,
        entry,
        environment,
        permissions,
    })
}

#[derive(Clone, Debug)]
struct ResolvedFilesystemPermission {
    location: String,
    source: PathBuf,
    target: PathBuf,
    access: FilesystemAccess,
    create: bool,
    implicit_xdg_create: bool,
}

#[derive(Clone, Debug)]
struct CandidateFilesystemPermission {
    location: String,
    requested_source: PathBuf,
    source: PathBuf,
    target: PathBuf,
    access: FilesystemAccess,
    create: bool,
    implicit_xdg_create: bool,
    mount_identities: BTreeSet<MountIdentity>,
    source_kind: Option<FilesystemSourceKind>,
    file_identity: Option<(u64, u64)>,
}

fn resolve_filesystem_grants(
    permissions: &PermissionPolicy,
    state: &StatePlan,
    package_files: &Path,
    runtime_files: &Path,
    wayland_socket: &Path,
    bus_socket: &Path,
) -> io::Result<Vec<FilesystemGrant>> {
    resolve_filesystem_grants_with_boundary(
        permissions,
        state,
        |mountinfo| {
            GrantBoundary::new(
                mountinfo,
                state,
                package_files,
                runtime_files,
                wayland_socket,
                bus_socket,
            )
        },
    )
}

fn resolve_filesystem_grants_with_boundary<F>(
    permissions: &PermissionPolicy,
    state: &StatePlan,
    boundary_for: F,
) -> io::Result<Vec<FilesystemGrant>>
where
    F: Fn(&str) -> io::Result<GrantBoundary>,
{
    let mut resolved = Vec::new();
    for (location, permission) in permissions.filesystems() {
        let (source, target, implicit_xdg_create) =
            filesystem_location(location, &state.real_home)?;
        validate_filesystem_target(&target)?;
        resolved.push(ResolvedFilesystemPermission {
            location: location.to_string(),
            source,
            target,
            access: permission.access(),
            create: permission.create(),
            implicit_xdg_create,
        });
    }
    if resolved.len() > MAX_FILESYSTEM_ENTRIES {
        return Err(invalid(format!(
            "application spec exceeds {MAX_FILESYSTEM_ENTRIES} filesystem permissions"
        )));
    }

    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let boundary = boundary_for(&mountinfo)?;
    let mut candidates = Vec::new();
    for permission in resolved {
        let source = canonical_candidate(&permission.source)?;
        let mount_identities = mount_tree_identities(&mountinfo, &source)?;
        if permission.access != FilesystemAccess::Deny {
            require_grant_target(&permission.target)?;
            boundary.require_source(&source, &mount_identities)?;
        }
        let source_metadata = preflight_filesystem_source(
            &source,
            permission.access,
            permission.create || permission.implicit_xdg_create,
        )?;
        candidates.push(CandidateFilesystemPermission {
            location: permission.location,
            requested_source: permission.source,
            source,
            target: permission.target,
            access: permission.access,
            create: permission.create,
            implicit_xdg_create: permission.implicit_xdg_create,
            mount_identities,
            source_kind: source_metadata.map(|metadata| metadata.source_kind),
            file_identity: source_metadata.and_then(|metadata| metadata.file_identity()),
        });
    }
    let denied = candidates
        .iter()
        .filter(|permission| permission.access == FilesystemAccess::Deny)
        .cloned()
        .collect::<Vec<_>>();
    let mut admitted: Vec<CandidateFilesystemPermission> = Vec::new();
    for candidate in candidates
        .into_iter()
        .filter(|permission| permission.access != FilesystemAccess::Deny)
    {
        if denied
            .iter()
            .any(|denied| filesystem_permissions_overlap(&candidate, denied))
        {
            continue;
        }
        let mut merged = false;
        for grant in &mut admitted {
            if grant.source == candidate.source && grant.target == candidate.target {
                if candidate.access == FilesystemAccess::ReadOnly {
                    grant.access = FilesystemAccess::ReadOnly;
                }
                grant.create |= candidate.create;
                grant.implicit_xdg_create |= candidate.implicit_xdg_create;
                merged = true;
                break;
            }
            if filesystem_permissions_overlap(grant, &candidate) {
                return Err(invalid(format!(
                    "filesystem permission {:?} aliases or overlaps another admitted grant",
                    candidate.location
                )));
            }
        }
        if !merged {
            admitted.push(candidate);
        }
    }

    for candidate in &admitted {
        if candidate.source_kind.is_none()
            && !(candidate.create || candidate.implicit_xdg_create)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "preflight filesystem permission source {} does not exist",
                    candidate.source.display()
                ),
            ));
        }
        if candidate.source_kind == Some(FilesystemSourceKind::File) {
            let metadata = fs::symlink_metadata(&candidate.source)?;
            require_filesystem_source_metadata(&candidate.source, &metadata, false, true)?;
        }
    }
    for candidate in &admitted {
        if candidate.create || candidate.implicit_xdg_create {
            ensure_grant_directory(&state.real_home, &candidate.requested_source)?;
        }
    }
    let post_mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    let post_boundary = boundary_for(&post_mountinfo)?;
    let mut post_denied = Vec::new();
    for mut denied in denied {
        denied.source = canonical_candidate(&denied.requested_source)?;
        denied.mount_identities = mount_tree_identities(&post_mountinfo, &denied.source)?;
        let metadata = preflight_filesystem_source(
            &denied.source,
            FilesystemAccess::Deny,
            false,
        )?;
        denied.source_kind = metadata.map(|metadata| metadata.source_kind);
        denied.file_identity = metadata.and_then(|metadata| metadata.file_identity());
        post_denied.push(denied);
    }
    let mut realized = Vec::<(FilesystemGrant, BTreeSet<MountIdentity>)>::new();
    for candidate in admitted {
        let source = fs::canonicalize(&candidate.requested_source).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "resolve filesystem permission {:?} at {}: {error}",
                    candidate.location,
                    candidate.requested_source.display()
                ),
            )
        })?;
        let mount_identities = mount_tree_identities(&post_mountinfo, &source)?;
        post_boundary.require_source(&source, &mount_identities)?;
        let metadata = fs::symlink_metadata(&source)?;
        let source_metadata = require_filesystem_source_metadata(
            &source,
            &metadata,
            candidate.create || candidate.implicit_xdg_create,
            false,
        )?;
        let realized_candidate = CandidateFilesystemPermission {
            source: source.clone(),
            mount_identities: mount_identities.clone(),
            source_kind: Some(source_metadata.source_kind),
            file_identity: source_metadata.file_identity(),
            ..candidate
        };
        if post_denied
            .iter()
            .any(|denied| filesystem_permissions_overlap(&realized_candidate, denied))
        {
            continue;
        }
        let source_kind = require_filesystem_source_metadata(
            &source,
            &metadata,
            realized_candidate.create || realized_candidate.implicit_xdg_create,
            true,
        )?
        .source_kind;
        let read_only = realized_candidate.access == FilesystemAccess::ReadOnly;
        let mut merged = false;
        for (grant, grant_mounts) in &mut realized {
            if grant.source == source && grant.target == realized_candidate.target {
                grant.read_only |= read_only;
                merged = true;
                break;
            }
            if paths_overlap(&grant.source, &source)
                || mount_identity_sets_overlap(grant_mounts, &mount_identities)
                || paths_overlap(&grant.target, &realized_candidate.target)
            {
                return Err(invalid(format!(
                    "filesystem permission {:?} aliases or overlaps another admitted grant after creation",
                    realized_candidate.location
                )));
            }
        }
        if merged {
            continue;
        }
        realized.push((
            FilesystemGrant {
                source,
                target: realized_candidate.target,
                read_only,
                source_kind,
                source_device: metadata.dev(),
                source_inode: metadata.ino(),
            },
            mount_identities,
        ));
    }
    let mut grants = realized
        .into_iter()
        .map(|(grant, _)| grant)
        .collect::<Vec<_>>();
    grants.sort_by(|left, right| left.target.cmp(&right.target));
    Ok(grants)
}

#[derive(Clone, Copy)]
struct FilesystemSourceMetadata {
    source_kind: FilesystemSourceKind,
    device: u64,
    inode: u64,
}

impl FilesystemSourceMetadata {
    fn file_identity(self) -> Option<(u64, u64)> {
        (self.source_kind == FilesystemSourceKind::File).then_some((self.device, self.inode))
    }
}

fn preflight_filesystem_source(
    source: &Path,
    access: FilesystemAccess,
    require_directory: bool,
) -> io::Result<Option<FilesystemSourceMetadata>> {
    match fs::symlink_metadata(source) {
        Ok(metadata) => {
            if access == FilesystemAccess::Deny
                && !metadata.file_type().is_dir()
                && !metadata.file_type().is_file()
            {
                return Ok(None);
            }
            require_filesystem_source_metadata(
                source,
                &metadata,
                require_directory,
                false,
            )
            .map(Some)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "preflight filesystem permission source {}: {error}",
                source.display()
            ),
        )),
    }
}

fn require_filesystem_source_metadata(
    source: &Path,
    metadata: &fs::Metadata,
    require_directory: bool,
    require_single_link: bool,
) -> io::Result<FilesystemSourceMetadata> {
    let source_kind = if metadata.file_type().is_dir() {
        FilesystemSourceKind::Directory
    } else if metadata.file_type().is_file() {
        if require_single_link && metadata.nlink() != 1 {
            return Err(invalid(format!(
                "filesystem permission source {} is a multiply-linked regular file",
                source.display()
            )));
        }
        FilesystemSourceKind::File
    } else {
        return Err(invalid(format!(
            "filesystem permission source {} is neither a directory nor a regular file",
            source.display()
        )));
    };
    if require_directory && source_kind != FilesystemSourceKind::Directory {
        return Err(invalid(format!(
            "filesystem create source {} is not a directory",
            source.display()
        )));
    }
    Ok(FilesystemSourceMetadata {
        source_kind,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn filesystem_permissions_overlap(
    left: &CandidateFilesystemPermission,
    right: &CandidateFilesystemPermission,
) -> bool {
    paths_overlap(&left.source, &right.source)
        || mount_identity_sets_overlap(&left.mount_identities, &right.mount_identities)
        || paths_overlap(&left.target, &right.target)
        || left.file_identity.is_some()
            && left.file_identity == right.file_identity
}

fn filesystem_location(location: &str, real_home: &Path) -> io::Result<(PathBuf, PathBuf, bool)> {
    let xdg = match location {
        "xdg-download" => Some("Downloads"),
        "xdg-documents" => Some("Documents"),
        "xdg-pictures" => Some("Pictures"),
        "xdg-music" => Some("Music"),
        "xdg-videos" => Some("Videos"),
        "xdg-desktop" => Some("Desktop"),
        _ => None,
    };
    if let Some(directory) = xdg {
        return Ok((
            real_home.join(directory),
            Path::new("/home/td").join(directory),
            true,
        ));
    }
    if let Some(relative) = location.strip_prefix("~/") {
        return Ok((
            real_home.join(relative),
            Path::new("/home/td").join(relative),
            false,
        ));
    }
    let absolute = PathBuf::from(location);
    if !absolute.is_absolute() {
        return Err(invalid(format!(
            "filesystem permission location {location:?} is not absolute"
        )));
    }
    Ok((absolute.clone(), absolute, false))
}

fn canonical_candidate(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    invalid(format!(
                        "filesystem permission path {} has no existing ancestor",
                        path.display()
                    ))
                })?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| invalid("filesystem permission path has no parent"))?
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
    let mut candidate = fs::canonicalize(existing)?;
    for component in missing.into_iter().rev() {
        candidate.push(component);
    }
    Ok(candidate)
}

fn ensure_grant_directory(real_home: &Path, requested: &Path) -> io::Result<()> {
    let relative = requested.strip_prefix(real_home).map_err(|_| {
        invalid(format!(
            "filesystem create target {} is outside the real home {}",
            requested.display(),
            real_home.display()
        ))
    })?;
    let mut current = real_home.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() {
                    return Err(invalid(format!(
                        "filesystem create component {} is not a directory",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        let canonical = fs::canonicalize(&current)?;
        if !path_is_same_or_child(&canonical, real_home) {
            return Err(invalid(format!(
                "filesystem create component {} resolves outside the real home {}",
                current.display(),
                real_home.display()
            )));
        }
        current = canonical;
    }
    Ok(())
}

struct GrantBoundary {
    reserved: Vec<PathBuf>,
    home_roots: Vec<PathBuf>,
    allowed_home: PathBuf,
    reserved_mounts: BTreeSet<MountIdentity>,
    home_mounts: BTreeSet<MountIdentity>,
    allowed_home_mounts: BTreeSet<MountIdentity>,
    other_home_mounts: BTreeSet<MountIdentity>,
}

impl GrantBoundary {
    fn new(
        mountinfo: &str,
        state: &StatePlan,
        package_files: &Path,
        runtime_files: &Path,
        wayland_socket: &Path,
        bus_socket: &Path,
    ) -> io::Result<Self> {
        let mut reserved = vec![
            state.state_root.clone(),
            state.runtime.clone(),
            canonical_candidate(&state.real_home.join(".local/share/flatpak"))?,
            package_files.to_path_buf(),
            runtime_files.to_path_buf(),
            wayland_socket.to_path_buf(),
            bus_socket.to_path_buf(),
        ];
        let mut home_roots = Vec::new();
        for path in RESERVED_FILESYSTEM_TREES {
            if matches!(*path, "/home" | "/var/home") {
                match fs::canonicalize(path) {
                    Ok(path) => {
                        if !home_roots.contains(&path) {
                            home_roots.push(path);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                continue;
            }
            match fs::canonicalize(path) {
                Ok(path) => reserved.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let reserved_mounts = mount_identities_for_roots(mountinfo, &reserved)?;
        let home_mounts = mount_identities_for_roots(mountinfo, &home_roots)?;
        let allowed_home_mounts = mount_tree_identities(mountinfo, &state.real_home)?;
        let other_home_mounts =
            mount_identities_outside_allowed_home(mountinfo, &home_roots, &state.real_home)?;
        Ok(Self {
            reserved,
            home_roots,
            allowed_home: state.real_home.clone(),
            reserved_mounts,
            home_mounts,
            allowed_home_mounts,
            other_home_mounts,
        })
    }

    #[cfg(test)]
    fn for_test(
        mountinfo: &str,
        state: &StatePlan,
        package_files: &Path,
        runtime_files: &Path,
        wayland_socket: &Path,
        bus_socket: &Path,
    ) -> io::Result<Self> {
        let reserved = vec![
            state.state_root.clone(),
            state.runtime.clone(),
            state.home.clone(),
            state.config.clone(),
            state.cache.clone(),
            state.data.clone(),
            state.local_state.clone(),
            package_files.to_path_buf(),
            runtime_files.to_path_buf(),
            wayland_socket.to_path_buf(),
            bus_socket.to_path_buf(),
        ];
        Ok(Self {
            reserved_mounts: mount_identities_for_roots(mountinfo, &reserved)?,
            allowed_home_mounts: mount_tree_identities(mountinfo, &state.real_home)?,
            reserved,
            home_roots: Vec::new(),
            allowed_home: state.real_home.clone(),
            home_mounts: BTreeSet::new(),
            other_home_mounts: BTreeSet::new(),
        })
    }

    fn require_source(
        &self,
        source: &Path,
        source_mounts: &BTreeSet<MountIdentity>,
    ) -> io::Result<()> {
        if let Some(path) = self.home_roots.iter().find(|home| {
            paths_overlap(source, home) && !path_is_same_or_child(source, &self.allowed_home)
        }) {
            return Err(invalid(format!(
                "filesystem permission source {} aliases another reserved home below {}",
                source.display(),
                path.display()
            )));
        }
        if let Some(path) = self
            .reserved
            .iter()
            .find(|reserved| paths_overlap(source, reserved))
        {
            return Err(invalid(format!(
                "filesystem permission source {} aliases reserved tree {}",
                source.display(),
                path.display()
            )));
        }
        require_grant_mount_identities(
            source,
            source_mounts,
            &self.reserved_mounts,
            &self.home_mounts,
            &self.allowed_home_mounts,
            &self.other_home_mounts,
        )
    }
}

fn require_grant_target(target: &Path) -> io::Result<()> {
    let private_home = Path::new("/home/td");
    if target == private_home || path_is_same_or_child(private_home, target) {
        return Err(invalid(format!(
            "filesystem permission target {} replaces the private home root",
            target.display()
        )));
    }
    for reserved in [
        "/app",
        "/usr",
        "/run",
        "/proc",
        "/sys",
        "/dev",
        "/tmp",
        "/var/tmp",
        "/etc",
        "/boot",
        "/.flatpak-info",
        "/oldroot",
        "/root-write-probe",
        "/home/td/.config",
        "/home/td/.cache",
        "/home/td/.local/share",
        "/home/td/.local/state",
    ] {
        if paths_overlap(target, Path::new(reserved)) {
            return Err(invalid(format!(
                "filesystem permission target {} overlaps reserved jail tree {reserved}",
                target.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn paths_overlap(left: &Path, right: &Path) -> bool {
    path_is_same_or_child(left, right) || path_is_same_or_child(right, left)
}

pub(crate) fn path_is_same_or_child(path: &Path, root: &Path) -> bool {
    path == root || path.strip_prefix(root).is_ok()
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MountIdentity {
    pub(crate) device: String,
    pub(crate) root: PathBuf,
}

fn mount_identities_overlap(left: &MountIdentity, right: &MountIdentity) -> bool {
    left.device == right.device && paths_overlap(&left.root, &right.root)
}

fn mount_identity_sets_overlap(
    left: &BTreeSet<MountIdentity>,
    right: &BTreeSet<MountIdentity>,
) -> bool {
    left.iter()
        .any(|left| right.iter().any(|right| mount_identities_overlap(left, right)))
}

pub(crate) fn require_grant_mount_identities(
    source: &Path,
    source_mounts: &BTreeSet<MountIdentity>,
    reserved_mounts: &BTreeSet<MountIdentity>,
    home_mounts: &BTreeSet<MountIdentity>,
    allowed_home_mounts: &BTreeSet<MountIdentity>,
    other_home_mounts: &BTreeSet<MountIdentity>,
) -> io::Result<()> {
    if mount_identity_sets_overlap(source_mounts, reserved_mounts) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "filesystem permission source {} aliases a reserved mount",
                source.display()
            ),
        ));
    }
    if mount_identity_sets_overlap(source_mounts, other_home_mounts) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "filesystem permission source {} aliases another home mount",
                source.display()
            ),
        ));
    }
    for source_mount in source_mounts {
        if home_mounts
            .iter()
            .any(|home| mount_identities_overlap(source_mount, home))
            && !allowed_home_mounts.iter().any(|allowed| {
                source_mount.device == allowed.device
                    && path_is_same_or_child(&source_mount.root, &allowed.root)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "filesystem permission source {} aliases another home mount",
                    source.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn mount_identities_outside_allowed_home(
    mountinfo: &str,
    home_roots: &[PathBuf],
    allowed_home: &Path,
) -> io::Result<BTreeSet<MountIdentity>> {
    let mut identities = BTreeSet::new();
    for line in mountinfo.lines() {
        let left = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?
            .0;
        let mountpoint = left
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| io::Error::other("mountinfo row has no mount point"))?;
        let mountpoint = decode_mountinfo_path(mountpoint)?;
        let below_home = home_roots
            .iter()
            .any(|home| path_is_same_or_child(&mountpoint, home));
        let belongs_to_allowed_home = path_is_same_or_child(&mountpoint, allowed_home)
            || path_is_same_or_child(allowed_home, &mountpoint);
        if below_home && !belongs_to_allowed_home {
            identities.insert(mount_identity_for_path(mountinfo, &mountpoint)?);
        }
    }
    Ok(identities)
}

fn mount_identities_for_roots(
    mountinfo: &str,
    roots: &[PathBuf],
) -> io::Result<BTreeSet<MountIdentity>> {
    let mut identities = BTreeSet::new();
    for root in roots {
        identities.extend(mount_tree_identities(mountinfo, root)?);
    }
    Ok(identities)
}

pub(crate) fn mount_tree_identities(
    mountinfo: &str,
    source: &Path,
) -> io::Result<BTreeSet<MountIdentity>> {
    let mut paths = BTreeSet::from([source.to_path_buf()]);
    for line in mountinfo.lines() {
        let left = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?
            .0;
        let mountpoint = left
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| io::Error::other("mountinfo row has no mount point"))?;
        let mountpoint = decode_mountinfo_path(mountpoint)?;
        if path_is_same_or_child(&mountpoint, source) {
            paths.insert(mountpoint);
        }
    }
    let mut identities = BTreeSet::new();
    for path in paths {
        identities.insert(mount_identity_for_path(mountinfo, &path)?);
    }
    Ok(identities)
}

#[derive(Clone, Debug)]
struct MountRow {
    id: u64,
    parent: u64,
    device: String,
    root: PathBuf,
    mountpoint: PathBuf,
}

fn root_mount_row(rows: &[MountRow]) -> Option<MountRow> {
    for row in rows.iter().rev() {
        if row.mountpoint == Path::new("/") {
            return Some(row.clone());
        }
    }
    None
}

fn child_mount_row(rows: &[MountRow], parent: u64) -> Option<MountRow> {
    for row in rows.iter().rev() {
        if row.parent == parent {
            return Some(row.clone());
        }
    }
    None
}

pub(crate) fn decode_mountinfo_path(field: &str) -> io::Result<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut bytes = field.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let escape = [bytes.next(), bytes.next(), bytes.next()];
        let value = match escape {
            [Some(b'0'), Some(b'4'), Some(b'0')] => b' ',
            [Some(b'0'), Some(b'1'), Some(b'1')] => b'\t',
            [Some(b'0'), Some(b'1'), Some(b'2')] => b'\n',
            [Some(b'1'), Some(b'3'), Some(b'4')] => b'\\',
            _ => {
                return Err(io::Error::other(
                    "mountinfo contains an invalid path escape",
                ));
            }
        };
        decoded.push(value);
    }
    Ok(PathBuf::from(OsString::from_vec(decoded)))
}

pub(crate) fn mount_identity_for_path(mountinfo: &str, path: &Path) -> io::Result<MountIdentity> {
    let mut rows = BTreeMap::<usize, Vec<MountRow>>::new();
    let mut mount_ids = BTreeSet::new();
    for line in mountinfo.lines() {
        let left = line
            .split_once(" - ")
            .ok_or_else(|| io::Error::other("mountinfo row has no separator"))?
            .0;
        let mut fields = left.split_whitespace();
        let id = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no mount ID"))?
            .parse::<u64>()
            .map_err(|error| io::Error::other(format!("invalid mount ID: {error}")))?;
        let parent = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no parent ID"))?
            .parse::<u64>()
            .map_err(|error| io::Error::other(format!("invalid mount parent ID: {error}")))?;
        if !mount_ids.insert(id) {
            return Err(io::Error::other(format!("mountinfo repeats mount ID {id}")));
        }
        let device = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no device"))?;
        let root = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no root"))?;
        let mountpoint = fields
            .next()
            .ok_or_else(|| io::Error::other("mountinfo row has no mount point"))?;
        let root = decode_mountinfo_path(root)?;
        let mountpoint = decode_mountinfo_path(mountpoint)?;
        if path.strip_prefix(&mountpoint).is_err() {
            continue;
        }
        let depth = mountpoint.components().count();
        rows.entry(depth).or_default().push(MountRow {
            id,
            parent,
            device: device.to_string(),
            root,
            mountpoint,
        });
    }
    let mut selected: Option<MountRow> = None;
    let mut selected_ids = BTreeSet::new();
    for rows_at_depth in rows.into_values() {
        if selected.is_none() {
            selected = root_mount_row(&rows_at_depth);
            if let Some(row) = &selected {
                selected_ids.insert(row.id);
            }
        }
        while let Some(parent) = selected.as_ref().map(|row| row.id) {
            let Some(next) = child_mount_row(&rows_at_depth, parent) else {
                break;
            };
            if !selected_ids.insert(next.id) {
                return Err(io::Error::other("mountinfo parent tree contains a cycle"));
            }
            selected = Some(next);
        }
    }
    let selected = selected
        .ok_or_else(|| io::Error::other(format!("mountinfo does not cover {}", path.display())))?;
    let relative = path.strip_prefix(&selected.mountpoint).map_err(|error| {
        io::Error::other(format!(
            "selected mount {} does not cover {}: {error}",
            selected.mountpoint.display(),
            path.display()
        ))
    })?;
    let mut root = selected.root;
    if !relative.as_os_str().is_empty() {
        root.push(relative);
    }
    Ok(MountIdentity {
        device: selected.device,
        root,
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

/// One uid-owned socket under the login user's runtime directory, resolved.
///
/// Checked before and after `canonicalize`, and it is worth being exact about
/// what each check does and does not buy, because a draft of this comment
/// claimed more than either delivers.
///
/// The FIRST check is the one that closes a case. `symlink_metadata` does not
/// follow the final component, so a `bus` that is a symlink to somebody else's
/// socket is seen as a link, is not a socket, and is refused here. Drop this
/// check and `canonicalize` resolves the link, the post-check sees a perfectly
/// good uid-owned socket at the far end, and the jail binds it.
///
/// The SECOND check closes no case of its own. Once the first has run there is
/// no symlink left for it to catch, and its test — type and owner — cannot tell
/// the right socket from any other socket the same uid owns. All it does is
/// narrow the window between the `symlink_metadata` lookup and `canonicalize`.
/// It is kept because narrowing that window is free and because the canonical
/// path is what gets bound, so verifying the name actually used costs nothing;
/// it is NOT a second boundary and a draft of this comment described it as one.
///
/// Neither closes a time-of-check-to-time-of-use gap. A process running as this
/// same uid that can write the runtime directory can swap the socket after both
/// checks and before the `mount`. What holds the bind to its source afterwards
/// is `require_bind_source`, which compares mount IDENTITIES out of
/// `/proc/self/mountinfo` once the bind exists rather than pathnames before it.
/// The residual is bounded by who can write `/run/user/<uid>`, which is 0700
/// and owned by the login user — the principal the session belongs to.
fn session_socket(name: &str, what: &str, uid: u32) -> io::Result<PathBuf> {
    let path = PathBuf::from(format!("/run/user/{uid}/{name}"));
    require_session_socket(&path, uid, what)?;
    // Labelled rather than propagated bare. The likeliest failure on this path
    // is now "the broker has not bound yet", and `main` prints `td-jail: {e}` —
    // so a bare `ENOENT` would reach an operator as `No such file or directory`
    // with no path and no noun, ambiguous between two sockets since there are
    // two. `require_regular` and `require_directory` in this file already say
    // which thing they were looking at; this is that habit applied here.
    let path = fs::canonicalize(&path).map_err(|error| {
        invalid(format!("{what} {}: {error}", path.display()))
    })?;
    require_session_socket(&path, uid, what)?;
    Ok(path)
}

fn require_session_socket(path: &Path, uid: u32, what: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| invalid(format!("{what} {}: {error}", path.display())))?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(invalid(format!(
            "{what} {} is not a socket owned by uid {uid}",
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
        real_home: home,
        state_root: applications,
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

/// The environment CONTRACT over a spec's own text, for the recipe check that
/// holds the engine and this crate to one value.
///
/// `test_parse_spec` is the grammar and nothing else, so a check built on it
/// alone passes while the two sides drift apart in value. This runs the same
/// `validate_environment_list` a launch runs, which is the only thing that can
/// notice.
#[cfg(test)]
pub(crate) fn test_validate_spec_environment(text: &str, uid: u32) -> io::Result<()> {
    let spec = parse_spec(text)?;
    let environment = spec
        .environment
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<Vec<_>>();
    validate_environment_list(&environment, uid)
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
    use crate::permissions::PermissionSocket;
    use std::ops::Deref;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> io::Result<Self> {
            loop {
                let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "td-jail-{label}-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    impl Deref for TestDirectory {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
        let text = "format=1\nname=fixture\nruntime=/td/store/0123456789abcdefghijklmnopqrstuv-empty-runtime-1\nentry=/app/bin/fixture\n\n[Environment]\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\nHOME=/home/td\nWAYLAND_DISPLAY=wayland-0\nXDG_RUNTIME_DIR=/run/user/1000\n\n[Context]\nsockets=wayland\n";
        assert!(test_parse_spec(text).is_ok());
        let spec = parse_spec(text).unwrap();
        assert_eq!(spec.name, "fixture");
        assert_eq!(spec.entry, "/app/bin/fixture");
        assert_eq!(spec.environment.len(), 4);

        let filesystem = text.replace(
            "sockets=wayland\n",
            "sockets=wayland\n\n[Filesystem]\nxdg-download=rw:create\nxdg-pictures=ro:create\n",
        );
        let spec = parse_spec(&filesystem).unwrap();
        assert_eq!(spec.permissions.filesystems().count(), 2);

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
            filesystem.replace(
                "xdg-download=rw:create\nxdg-pictures=ro:create",
                "xdg-pictures=ro:create\nxdg-download=rw:create",
            ),
        ] {
            assert!(parse_spec(&invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn filesystem_grants_resolve_create_merge_deny_and_reserved_aliases() {
        use std::os::unix::fs::symlink;

        let root = TestDirectory::new("authority-grants").unwrap();
        for directory in [
            root.join("home"),
            root.join("home/state-root"),
            root.join("private-home"),
            root.join("config"),
            root.join("cache"),
            root.join("data"),
            root.join("local-state"),
            root.join("runtime-state"),
            root.join("package"),
            root.join("runtime"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        let real_home = fs::canonicalize(root.join("home")).unwrap();
        let state_root = fs::canonicalize(root.join("home/state-root")).unwrap();
        let state = StatePlan {
            real_home: real_home.clone(),
            state_root: state_root.clone(),
            home: fs::canonicalize(root.join("private-home")).unwrap(),
            config: fs::canonicalize(root.join("config")).unwrap(),
            cache: fs::canonicalize(root.join("cache")).unwrap(),
            data: fs::canonicalize(root.join("data")).unwrap(),
            local_state: fs::canonicalize(root.join("local-state")).unwrap(),
            runtime: fs::canonicalize(root.join("runtime-state")).unwrap(),
        };
        let package = fs::canonicalize(root.join("package")).unwrap();
        let runtime = fs::canonicalize(root.join("runtime")).unwrap();
        let wayland = root.join("wayland-0");
        fs::write(&wayland, b"socket-placeholder").unwrap();
        let bus = root.join("bus");
        fs::write(&bus, b"socket-placeholder").unwrap();
        let resolve = |policy: &PermissionPolicy| {
            resolve_filesystem_grants_with_boundary(policy, &state, |mountinfo| {
                GrantBoundary::for_test(
                    mountinfo, &state, &package, &runtime, &wayland, &bus,
                )
            })
        };

        let policy = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("xdg-download", FilesystemAccess::ReadWrite, false)
            .unwrap()
            .with_filesystem("~/Downloads", FilesystemAccess::ReadOnly, false)
            .unwrap()
            .with_filesystem("xdg-pictures", FilesystemAccess::ReadOnly, true)
            .unwrap();
        let grants = resolve(&policy).unwrap();
        assert_eq!(grants.len(), 2);
        let download = grants.first().unwrap();
        assert_eq!(download.target, Path::new("/home/td/Downloads"));
        assert!(download.read_only);
        let pictures = grants.get(1).unwrap();
        assert_eq!(pictures.target, Path::new("/home/td/Pictures"));
        assert!(pictures.read_only);
        assert!(real_home.join("Downloads").is_dir());
        assert!(real_home.join("Pictures").is_dir());

        let denied = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("xdg-download", FilesystemAccess::ReadWrite, false)
            .unwrap()
            .with_filesystem("~/Downloads/private", FilesystemAccess::Deny, false)
            .unwrap();
        assert!(
            resolve(&denied).unwrap().is_empty()
        );

        let denied_create = real_home.join("DeniedCreate");
        let denied = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/DeniedCreate", FilesystemAccess::ReadWrite, true)
            .unwrap()
            .with_filesystem("~/DeniedCreate/private", FilesystemAccess::Deny, false)
            .unwrap();
        assert!(
            resolve(&denied).unwrap().is_empty()
        );
        assert!(!denied_create.exists());

        let reserved_create = state_root.join("new");
        let reserved = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/state-root/new", FilesystemAccess::ReadWrite, true)
            .unwrap();
        assert!(resolve(&reserved)
            .unwrap_err()
            .to_string()
            .contains("aliases reserved tree"));
        assert!(!reserved_create.exists());

        let overlapping = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/Projects", FilesystemAccess::ReadOnly, true)
            .unwrap()
            .with_filesystem("~/Projects/code", FilesystemAccess::ReadOnly, true)
            .unwrap();
        assert!(
            resolve(&overlapping).is_err()
        );

        symlink(&state_root, real_home.join("state-alias")).unwrap();
        let reserved = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/state-alias", FilesystemAccess::ReadOnly, false)
            .unwrap();
        assert!(
            resolve(&reserved)
                .unwrap_err()
                .to_string()
                .contains("aliases reserved tree")
        );

        let private_state = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/.config", FilesystemAccess::ReadOnly, true)
            .unwrap();
        assert!(
            resolve(&private_state)
                .unwrap_err()
                .to_string()
                .contains("overlaps reserved jail tree")
        );
        assert!(!real_home.join(".config").exists());

        let preflight_create = real_home.join("A-PreflightCreate");
        let invalid_after_create = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem(
                "~/A-PreflightCreate",
                FilesystemAccess::ReadWrite,
                true,
            )
            .unwrap()
            .with_filesystem("~/Z-MissingRequired", FilesystemAccess::ReadOnly, false)
            .unwrap();
        assert!(resolve(&invalid_after_create)
            .unwrap_err()
            .to_string()
            .contains("preflight filesystem permission source"));
        assert!(!preflight_create.exists());

        let nondirectory = real_home.join("NotDirectory");
        fs::write(&nondirectory, b"file").unwrap();
        let invalid_create_kind = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/NotDirectory", FilesystemAccess::ReadWrite, true)
            .unwrap();
        assert!(resolve(&invalid_create_kind)
            .unwrap_err()
            .to_string()
            .contains("is not a directory"));

        let special_source = real_home.join("SpecialSocket");
        let _listener = UnixListener::bind(&special_source).unwrap();
        let special_create = real_home.join("SpecialCreate");
        let invalid_special = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/SpecialCreate", FilesystemAccess::ReadWrite, true)
            .unwrap()
            .with_filesystem("~/SpecialSocket", FilesystemAccess::ReadOnly, false)
            .unwrap();
        assert!(resolve(&invalid_special)
            .unwrap_err()
            .to_string()
            .contains("is neither a directory nor a regular file"));
        assert!(!special_create.exists());

        let single_file = real_home.join("SingleFile");
        fs::write(&single_file, b"single").unwrap();
        let single_file_policy = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/SingleFile", FilesystemAccess::ReadOnly, false)
            .unwrap();
        let single_file_grants = resolve(&single_file_policy).unwrap();
        assert_eq!(single_file_grants.len(), 1);
        assert_eq!(
            single_file_grants.first().unwrap().source_kind,
            FilesystemSourceKind::File
        );

        let denied_file = real_home.join("DeniedFile");
        let allowed_file = real_home.join("AllowedFile");
        fs::write(&denied_file, b"denied").unwrap();
        fs::hard_link(&denied_file, &allowed_file).unwrap();
        let hardlink_deny = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/AllowedFile", FilesystemAccess::ReadOnly, false)
            .unwrap()
            .with_filesystem("~/DeniedFile", FilesystemAccess::Deny, false)
            .unwrap();
        assert!(resolve(&hardlink_deny).unwrap().is_empty());

        let admitted_hardlink = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/AllowedFile", FilesystemAccess::ReadOnly, false)
            .unwrap();
        assert!(resolve(&admitted_hardlink)
            .unwrap_err()
            .to_string()
            .contains("multiply-linked regular file"));

        let late_hardlink = real_home.join("Z-Hardlink");
        fs::hard_link(&denied_file, &late_hardlink).unwrap();
        let hardlink_create = real_home.join("A-HardlinkCreate");
        let invalid_hardlink_after_create = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("~/A-HardlinkCreate", FilesystemAccess::ReadWrite, true)
            .unwrap()
            .with_filesystem("~/Z-Hardlink", FilesystemAccess::ReadOnly, false)
            .unwrap();
        assert!(resolve(&invalid_hardlink_after_create)
            .unwrap_err()
            .to_string()
            .contains("multiply-linked regular file"));
        assert!(!hardlink_create.exists());

        let reserved_file = state_root.join("reserved-file");
        let reserved_file_alias = real_home.join("reserved-file-alias");
        fs::write(&reserved_file, b"reserved").unwrap();
        fs::hard_link(&reserved_file, &reserved_file_alias).unwrap();
        let hardlink_reserved = PermissionPolicy::new()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem(
                "~/reserved-file-alias",
                FilesystemAccess::ReadOnly,
                false,
            )
            .unwrap();
        assert!(resolve(&hardlink_reserved)
            .unwrap_err()
            .to_string()
            .contains("multiply-linked regular file"));
    }

    #[test]
    fn filesystem_grant_targets_reserve_stage2_internal_paths() {
        for target in ["/oldroot", "/oldroot/child", "/root-write-probe"] {
            assert!(require_grant_target(Path::new(target)).is_err());
        }
    }

    #[test]
    fn grant_overlap_includes_underlying_mount_identity() {
        let candidate = |source: &str, target: &str, root: &str| {
            CandidateFilesystemPermission {
                location: source.into(),
                requested_source: PathBuf::from(source),
                source: PathBuf::from(source),
                target: PathBuf::from(target),
                access: FilesystemAccess::ReadOnly,
                create: false,
                implicit_xdg_create: false,
                mount_identities: BTreeSet::from([MountIdentity {
                    device: "8:1".into(),
                    root: PathBuf::from(root),
                }]),
                source_kind: Some(FilesystemSourceKind::Directory),
                file_identity: None,
            }
        };
        let direct = candidate("/home/test/Pictures", "/home/td/Pictures", "/home/test/Pictures");
        let alias = candidate("/mnt/pics", "/mnt/pics", "/home/test/Pictures");
        let sibling = candidate("/mnt/music", "/mnt/music", "/home/test/Music");
        assert!(filesystem_permissions_overlap(&direct, &alias));
        assert!(!filesystem_permissions_overlap(&direct, &sibling));
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
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from("unix:path=/run/user/1000/bus"),
            ),
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
        // A DIFFERENT uid fails on every uid-derived value at once, the bus
        // address among them: an app told to reach /run/user/1000/bus while
        // running as 1001 would find a socket it cannot use, or somebody
        // else's.
        assert!(validate_environment_list(&environment, 1001).is_err());
        // Each required name is required ON ITS OWN. A draft sliced the front
        // of the array instead, which drops one name and then two, so the
        // second assertion passed for the first one's reason and no name after
        // `FLATPAK_ID` was ever exercised alone.
        for dropped in [
            "DBUS_SESSION_BUS_ADDRESS",
            "FLATPAK_ID",
            "HOME",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
        ] {
            let without = environment
                .iter()
                .filter(|(key, _)| key != dropped)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(without.len(), environment.len() - 1, "{dropped} was not there");
            assert!(
                validate_environment_list(&without, 1000).is_err(),
                "a spec missing {dropped} was accepted"
            );
        }

        // A bus address that is well formed and points somewhere ELSE is the
        // case the uid check cannot see: same uid, different socket. It is
        // refused because the address is not advice — td-jail binds one socket
        // into the jail at one path, and a spec naming another either sends the
        // app to a socket the mount namespace has no bind for, or names a bus
        // outside the runtime directory that the app has no business reaching.
        // The value is checked, not merely the key's presence.
        let elsewhere = environment
            .iter()
            .map(|(key, value)| {
                if key == "DBUS_SESSION_BUS_ADDRESS" {
                    (key.clone(), OsString::from("unix:path=/tmp/bus"))
                } else {
                    (key.clone(), value.clone())
                }
            })
            .collect::<Vec<_>>();
        assert!(
            validate_environment_list(&elsewhere, 1000).is_err(),
            "a spec may not point the app at a bus this jail does not mount"
        );

        let unsorted = [
            (
                OsString::from("WAYLAND_DISPLAY"),
                OsString::from("wayland-0"),
            ),
            (OsString::from("HOME"), OsString::from("/home/td")),
        ];
        assert!(validate_environment_list(&unsorted, 1000).is_err());
    }

    /// The shim the td-jail recipe check reaches across the crate boundary
    /// with, exercised on THIS side too.
    ///
    /// Not because that check would miss a broken shim. A draft of this comment
    /// said it could not tell a working shim from one that answers `Ok` to
    /// everything, and that is false: the negative case beside it — the same
    /// spec with the bus moved one path over — fails an always-`Ok` shim.
    ///
    /// What the recipe check cannot do is exercise the contract against a spec
    /// the ENGINE does not compile. The uid it is evaluated against is the
    /// shim's own argument and no compiled spec varies it, so the uid case
    /// below exists only here; and the wiring — text through `parse_spec` into
    /// `validate_environment_list` — is pinned on the side that owns both.
    #[test]
    fn the_spec_environment_shim_runs_the_contract_it_names() {
        let text = "format=1\nname=fixture\nruntime=/td/store/0123456789abcdefghijklmnopqrstuv-empty-runtime-1\nentry=/app/bin/fixture\n\n[Environment]\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\nFLATPAK_ID=org.td.Fixture\nHOME=/home/td\nWAYLAND_DISPLAY=wayland-0\nXDG_RUNTIME_DIR=/run/user/1000\n\n[Context]\nsockets=wayland\n";
        assert!(
            test_validate_spec_environment(text, 1000).is_ok(),
            "a complete spec must be accepted"
        );
        assert!(
            test_validate_spec_environment(text, 1001).is_err(),
            "the shim must evaluate the contract against the uid it is given"
        );
        assert!(
            test_validate_spec_environment(
                &text.replace("/run/user/1000/bus", "/run/user/1000/elsewhere"),
                1000,
            )
            .is_err(),
            "the shim must reach the VALUE check and not stop at the grammar"
        );
    }
}
