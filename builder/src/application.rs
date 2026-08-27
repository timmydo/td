//! Offline admission for a foreign dynamically linked application.
//!
//! This module models only the namespace td-jail assembles: an application at
//! `/app`, its selected runtime at `/usr`, and the four conventional aliases
//! into `/usr`. It never executes imported bytes and never asks the host
//! loader to resolve an edge.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const MAX_NAMESPACE_PATH_BYTES: usize = 4096;
const MAX_TREE_ENTRIES: usize = 262_144;
const MAX_LINK_TRAVERSALS: usize = 64;
const MAX_LOADER_OBJECTS: usize = 32_768;
const MAX_LOADER_EDGES: usize = 262_144;
const MAX_LOADER_STATE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LOADER_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_RPATH_ANCESTORS: usize = 64;
const MAX_RPATH_ANCESTRY_BYTES: usize = 64 * 1024;
const MAX_SHEBANG_BYTES: usize = 256;
const MAX_DYNAMIC_ELF_BYTES: u64 = td_engine::ostree::MAX_ARCHIVE_FILE_BYTES as u64;
const MAX_DYNAMIC_STRING_BYTES: usize = 64 * 1024;
const MAX_DYNAMIC_TEXT_BYTES: usize = 16 * 1024 * 1024;

const RUNTIME_LIBRARY_PATHS: &[&str] = &["/usr/lib/x86_64-linux-gnu", "/usr/lib64", "/usr/lib"];
const APPLICATION_ALIASES: &[(&str, &str)] = &[
    ("bin", "/usr/bin"),
    ("lib", "/usr/lib"),
    ("lib64", "/usr/lib64"),
    ("sbin", "/usr/sbin"),
];

#[derive(Debug)]
struct Namespace<'a> {
    app: &'a Path,
    usr: &'a Path,
}

#[derive(Debug)]
struct ResolvedPath {
    virtual_path: String,
    physical_path: PathBuf,
}

#[derive(Debug)]
enum ResolveOutcome {
    Found(ResolvedPath),
    Missing(String),
}

#[derive(Debug)]
enum PendingComponent {
    Parent,
    Name(String),
}

impl Namespace<'_> {
    fn physical(&self, virtual_path: &str) -> Result<PathBuf, String> {
        if virtual_path == "/app" {
            return Ok(self.app.to_path_buf());
        }
        if virtual_path == "/usr" {
            return Ok(self.usr.to_path_buf());
        }
        if let Some(relative) = virtual_path.strip_prefix("/app/") {
            return Ok(self.app.join(relative));
        }
        if let Some(relative) = virtual_path.strip_prefix("/usr/") {
            return Ok(self.usr.join(relative));
        }
        Err(format!(
            "dynamic application path {virtual_path:?} is outside /app and /usr"
        ))
    }

    fn resolve(&self, requested: &str) -> Result<ResolveOutcome, String> {
        let (absolute, mut pending) = pending_components(requested)?;
        if !absolute {
            return Err(format!(
                "dynamic application path {requested:?} is not absolute"
            ));
        }
        let mut resolved = Vec::new();
        let mut traversals = 0usize;
        while let Some(component) = pending.pop_front() {
            match component {
                PendingComponent::Parent => {
                    if resolved.pop().is_none() {
                        return Err(format!(
                            "dynamic application path {requested:?} escapes the assembled root"
                        ));
                    }
                }
                PendingComponent::Name(name) => {
                    if resolved.is_empty() {
                        if let Some(target) = application_alias(&name) {
                            account_link_traversal(&mut traversals, requested)?;
                            let (_, target_components) = pending_components(target)?;
                            prepend_components(&mut pending, target_components);
                            continue;
                        }
                    }
                    resolved.push(name);
                    let virtual_path = canonical_virtual(&resolved)?;
                    let physical = self.physical(&virtual_path)?;
                    let metadata = match fs::symlink_metadata(&physical) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(ResolveOutcome::Missing(finish_missing_path(
                                resolved, pending,
                            )?));
                        }
                        Err(error) => {
                            return Err(format!(
                                "inspect dynamic application path {}: {error}",
                                physical.display()
                            ));
                        }
                    };
                    if metadata.file_type().is_symlink() {
                        account_link_traversal(&mut traversals, requested)?;
                        let target = fs::read_link(&physical).map_err(|error| {
                            format!(
                                "read dynamic application symlink {}: {error}",
                                physical.display()
                            )
                        })?;
                        let target = target.to_str().ok_or_else(|| {
                            format!(
                                "dynamic application symlink {} has a non-UTF-8 target",
                                physical.display()
                            )
                        })?;
                        resolved.pop();
                        let (absolute, target_components) = pending_components(target)?;
                        if absolute {
                            resolved.clear();
                        }
                        prepend_components(&mut pending, target_components);
                        continue;
                    }
                    if !pending.is_empty() && !metadata.file_type().is_dir() {
                        return Err(format!(
                            "dynamic application path {} traverses a non-directory",
                            physical.display()
                        ));
                    }
                }
            }
        }
        let virtual_path = canonical_virtual(&resolved)?;
        Ok(ResolveOutcome::Found(ResolvedPath {
            physical_path: self.physical(&virtual_path)?,
            virtual_path,
        }))
    }
}

fn application_alias(name: &str) -> Option<&'static str> {
    APPLICATION_ALIASES
        .iter()
        .find_map(|(alias, target)| (*alias == name).then_some(*target))
}

fn validate_runtime_alias_targets(namespace: &Namespace<'_>) -> Result<(), String> {
    for (alias, target) in APPLICATION_ALIASES {
        let resolved = match namespace.resolve(target)? {
            ResolveOutcome::Found(resolved) => resolved,
            ResolveOutcome::Missing(path) => {
                return Err(format!(
                    "dynamic application runtime alias /{alias} has no target {path}"
                ));
            }
        };
        let metadata = fs::metadata(&resolved.physical_path).map_err(|error| {
            format!(
                "inspect dynamic application runtime alias /{alias} target {}: {error}",
                resolved.physical_path.display()
            )
        })?;
        if !metadata.file_type().is_dir() {
            return Err(format!(
                "dynamic application runtime alias /{alias} target {} is not a directory",
                resolved.virtual_path
            ));
        }
    }
    Ok(())
}

fn account_link_traversal(traversals: &mut usize, requested: &str) -> Result<(), String> {
    *traversals = traversals
        .checked_add(1)
        .ok_or("dynamic application symlink traversal count overflow")?;
    if *traversals > MAX_LINK_TRAVERSALS {
        return Err(format!(
            "dynamic application path {requested:?} exceeds {MAX_LINK_TRAVERSALS} symlink traversals"
        ));
    }
    Ok(())
}

fn pending_components(path: &str) -> Result<(bool, VecDeque<PendingComponent>), String> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err("dynamic application path is empty or contains NUL".into());
    }
    let absolute = path.starts_with('/');
    let mut pending = VecDeque::new();
    for component in Path::new(path).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => pending.push_back(PendingComponent::Parent),
            Component::Normal(name) => pending.push_back(PendingComponent::Name(
                name.to_str()
                    .ok_or("dynamic application path is not UTF-8")?
                    .to_string(),
            )),
            Component::Prefix(_) => {
                return Err("dynamic application path has a platform prefix".into());
            }
        }
    }
    Ok((absolute, pending))
}

fn prepend_components(
    pending: &mut VecDeque<PendingComponent>,
    prefix: VecDeque<PendingComponent>,
) {
    for component in prefix.into_iter().rev() {
        pending.push_front(component);
    }
}

fn finish_missing_path(
    mut resolved: Vec<String>,
    mut pending: VecDeque<PendingComponent>,
) -> Result<String, String> {
    while let Some(component) = pending.pop_front() {
        match component {
            PendingComponent::Parent => {
                if resolved.pop().is_none() {
                    return Err(
                        "dynamic application missing path escapes the assembled root".into(),
                    );
                }
            }
            PendingComponent::Name(name) => resolved.push(name),
        }
    }
    canonical_virtual(&resolved)
}

fn canonical_virtual(components: &[String]) -> Result<String, String> {
    let normalized = format!("/{}", components.join("/"));
    if normalized.len() > MAX_NAMESPACE_PATH_BYTES {
        return Err(format!(
            "dynamic application path exceeds {MAX_NAMESPACE_PATH_BYTES} bytes"
        ));
    }
    if normalized == "/run/host" || normalized.starts_with("/run/host/") {
        return Err("dynamic application path resolves through forbidden /run/host".into());
    }
    if normalized != "/app"
        && normalized != "/usr"
        && !normalized.starts_with("/app/")
        && !normalized.starts_with("/usr/")
    {
        return Err(format!(
            "dynamic application path {normalized:?} is outside the assembled namespace"
        ));
    }
    Ok(normalized)
}

fn virtual_components(path: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => out.push(
                name.to_str()
                    .ok_or("dynamic application path is not UTF-8")?
                    .to_string(),
            ),
            _ => {
                return Err(format!(
                    "dynamic application path {path:?} is not canonical"
                ));
            }
        }
    }
    Ok(out)
}

fn normalize_virtual(base: &str, target: &str) -> Result<String, String> {
    if target.is_empty() || target.as_bytes().contains(&0) {
        return Err("dynamic application path is empty or contains NUL".into());
    }
    let mut components = if target.starts_with('/') {
        Vec::new()
    } else {
        virtual_components(base)?
    };
    for component in Path::new(target).components() {
        match component {
            Component::RootDir => components.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                if components.pop().is_none() {
                    return Err(format!(
                        "dynamic application path {target:?} escapes the assembled root"
                    ));
                }
            }
            Component::Normal(name) => components.push(
                name.to_str()
                    .ok_or("dynamic application path is not UTF-8")?
                    .to_string(),
            ),
            Component::Prefix(_) => {
                return Err("dynamic application path has a platform prefix".into());
            }
        }
    }
    if let Some(target) = components
        .first()
        .and_then(|first| application_alias(first))
    {
        let mut rewritten = virtual_components(target)?;
        rewritten.extend(components.into_iter().skip(1));
        components = rewritten;
    }
    let normalized = format!("/{}", components.join("/"));
    if normalized.len() > MAX_NAMESPACE_PATH_BYTES {
        return Err(format!(
            "dynamic application path exceeds {MAX_NAMESPACE_PATH_BYTES} bytes"
        ));
    }
    if normalized == "/run/host" || normalized.starts_with("/run/host/") {
        return Err("dynamic application path resolves through forbidden /run/host".into());
    }
    if normalized != "/app"
        && normalized != "/usr"
        && !normalized.starts_with("/app/")
        && !normalized.starts_with("/usr/")
    {
        return Err(format!(
            "dynamic application path {normalized:?} is outside the assembled namespace"
        ));
    }
    Ok(normalized)
}

fn validate_tree_root(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{label} {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{label} {} is not a directory", path.display()));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o7000 != 0 || mode & 0o001 == 0 {
        return Err(format!(
            "{label} {} retains special mode bits or is not world-traversable",
            path.display()
        ));
    }
    Ok(())
}

fn is_same_or_child(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn validate_optional_roots(roots: &[String]) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for root in roots {
        let normalized = normalize_virtual("/", root)?;
        if normalized != *root || !root.starts_with("/app/") {
            return Err(format!(
                "optional dynamic application target {root:?} is not one canonical /app path"
            ));
        }
        if previous.is_some_and(|value| value >= root.as_str()) {
            return Err("optional dynamic application targets are not sorted and unique".into());
        }
        previous = Some(root);
    }
    Ok(())
}

fn validate_library_paths(paths: &[String]) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for path in paths {
        let normalized = normalize_virtual("/", path)?;
        if normalized != *path || !path.starts_with("/app/") {
            return Err(format!(
                "dynamic application library path {path:?} is not one canonical /app path"
            ));
        }
        if previous.is_some_and(|value| value >= path.as_str()) {
            return Err("dynamic application library paths are not sorted and unique".into());
        }
        previous = Some(path);
    }
    Ok(())
}

fn collect_application_files(
    namespace: &Namespace<'_>,
    optional_targets: &[String],
) -> Result<(Vec<(String, PathBuf)>, usize), String> {
    let mut files = Vec::new();
    let mut missing_optional_links = 0usize;
    let mut pending = vec![("/app".to_string(), namespace.app.to_path_buf())];
    let mut entries = 0usize;
    while let Some((virtual_directory, physical_directory)) = pending.pop() {
        let children = fs::read_dir(&physical_directory).map_err(|error| {
            format!(
                "read dynamic application directory {}: {error}",
                physical_directory.display()
            )
        })?;
        for child in children {
            let child = child.map_err(|error| {
                format!(
                    "read dynamic application directory {}: {error}",
                    physical_directory.display()
                )
            })?;
            entries = entries
                .checked_add(1)
                .ok_or("dynamic application tree entry count overflow")?;
            if entries > MAX_TREE_ENTRIES {
                return Err(format!(
                    "dynamic application tree exceeds {MAX_TREE_ENTRIES} entries"
                ));
            }
            let name = child.file_name().into_string().map_err(|_| {
                format!(
                    "dynamic application directory {} has a non-UTF-8 child",
                    physical_directory.display()
                )
            })?;
            let virtual_path = format!("{virtual_directory}/{name}");
            if virtual_path.len() > MAX_NAMESPACE_PATH_BYTES {
                return Err(format!(
                    "dynamic application path exceeds {MAX_NAMESPACE_PATH_BYTES} bytes"
                ));
            }
            let physical_path = child.path();
            let metadata = fs::symlink_metadata(&physical_path).map_err(|error| {
                format!(
                    "inspect dynamic application path {}: {error}",
                    physical_path.display()
                )
            })?;
            let mode = metadata.permissions().mode();
            if mode & 0o7000 != 0 {
                return Err(format!(
                    "dynamic application path {} retains special mode bits",
                    physical_path.display()
                ));
            }
            if metadata.file_type().is_dir() {
                if mode & 0o001 == 0 {
                    return Err(format!(
                        "dynamic application directory {} is not world-traversable",
                        physical_path.display()
                    ));
                }
                pending.push((virtual_path, physical_path));
            } else if metadata.file_type().is_file() {
                if files.len() >= MAX_LOADER_OBJECTS {
                    return Err(format!(
                        "dynamic application tree exceeds {MAX_LOADER_OBJECTS} regular files"
                    ));
                }
                files.push((virtual_path, physical_path));
            } else if metadata.file_type().is_symlink() {
                if let ResolveOutcome::Missing(normalized) = namespace.resolve(&virtual_path)? {
                    if !optional_targets
                        .iter()
                        .any(|root| is_same_or_child(&normalized, root))
                    {
                        return Err(format!(
                            "dynamic application symlink {virtual_path} has missing target {normalized}"
                        ));
                    }
                    missing_optional_links = missing_optional_links
                        .checked_add(1)
                        .ok_or("optional dynamic application link count overflow")?;
                }
            } else {
                return Err(format!(
                    "dynamic application path {} is not a directory, regular file, or symlink",
                    physical_path.display()
                ));
            }
        }
    }
    Ok((files, missing_optional_links))
}

fn account_loader_edge(edges: &mut usize) -> Result<(), String> {
    *edges = edges
        .checked_add(1)
        .ok_or("dynamic application loader edge count overflow")?;
    if *edges > MAX_LOADER_EDGES {
        return Err(format!(
            "dynamic application loader graph exceeds {MAX_LOADER_EDGES} edges"
        ));
    }
    Ok(())
}

fn account_loader_state(
    retained_bytes: &mut usize,
    object_path: &str,
    inherited_rpaths: &[String],
) -> Result<(), String> {
    let state_bytes = inherited_rpaths
        .iter()
        .try_fold(object_path.len(), |total, path| {
            total
                .checked_add(path.len())
                .ok_or("dynamic application loader-state size overflows")
        })?;
    *retained_bytes = retained_bytes
        .checked_add(state_bytes)
        .ok_or("dynamic application loader-state aggregate overflows")?;
    if *retained_bytes > MAX_LOADER_STATE_BYTES {
        return Err(format!(
            "dynamic application loader states exceed {MAX_LOADER_STATE_BYTES} retained bytes"
        ));
    }
    Ok(())
}

fn account_loader_file_bytes(total: &mut u64, bytes: u64) -> Result<(), String> {
    *total = total
        .checked_add(bytes)
        .ok_or("dynamic application loader-file aggregate overflows")?;
    if *total > MAX_LOADER_FILE_BYTES {
        return Err(format!(
            "dynamic application loader files exceed {MAX_LOADER_FILE_BYTES} aggregate bytes"
        ));
    }
    Ok(())
}

fn executable_file(path: &ResolvedPath, label: &str) -> Result<(), String> {
    let metadata = fs::metadata(&path.physical_path)
        .map_err(|error| format!("{label} {}: {error}", path.physical_path.display()))?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o001 == 0 {
        return Err(format!(
            "{label} {} is not a world-executable regular file",
            path.physical_path.display()
        ));
    }
    Ok(())
}

fn validate_entry(namespace: &Namespace<'_>, entry: &str) -> Result<Option<ResolvedPath>, String> {
    let normalized = normalize_virtual("/", entry)?;
    if normalized != entry || !entry.starts_with("/app/") {
        return Err(format!(
            "dynamic application entry {entry:?} is not one canonical /app path"
        ));
    }
    let resolved_entry = match namespace.resolve(entry)? {
        ResolveOutcome::Found(resolved) => resolved,
        ResolveOutcome::Missing(_) => {
            return Err(format!("dynamic application entry {entry:?} is absent"));
        }
    };
    let direct = namespace.physical(entry)?;
    if resolved_entry.virtual_path != entry || resolved_entry.physical_path != direct {
        return Err(format!(
            "dynamic application entry {entry:?} is not a direct /app file"
        ));
    }
    let direct_metadata = fs::symlink_metadata(&resolved_entry.physical_path).map_err(|error| {
        format!(
            "dynamic application entry {}: {error}",
            resolved_entry.physical_path.display()
        )
    })?;
    if !direct_metadata.file_type().is_file() || direct_metadata.permissions().mode() & 0o001 == 0 {
        return Err(format!(
            "dynamic application entry {} is not a directly executable regular file",
            direct.display()
        ));
    }
    let mut file = fs::File::open(&resolved_entry.physical_path).map_err(|error| {
        format!(
            "open dynamic application entry {}: {error}",
            direct.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SHEBANG_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "read dynamic application entry {}: {error}",
                direct.display()
            )
        })?;
    if bytes.starts_with(b"\x7fELF") {
        crate::elf::assert_x86_64_executable_bounded(
            &resolved_entry.physical_path,
            MAX_DYNAMIC_ELF_BYTES,
        )
        .map_err(|error| {
            format!(
                "dynamic application entry {}: {error}",
                resolved_entry.physical_path.display()
            )
        })?;
        return Ok(None);
    }
    if !bytes.starts_with(b"#!") {
        return Err(format!(
            "dynamic application entry {} is neither ELF nor a shebang script",
            direct.display()
        ));
    }
    let line_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| {
            format!(
                "dynamic application entry {} has no bounded shebang line",
                direct.display()
            )
        })?;
    if line_end > MAX_SHEBANG_BYTES
        || bytes
            .get(..line_end)
            .is_some_and(|line| line.contains(&0) || line.contains(&b'\r'))
    {
        return Err(format!(
            "dynamic application entry {} has an invalid shebang line",
            direct.display()
        ));
    }
    let line = std::str::from_utf8(
        bytes
            .get(2..line_end)
            .ok_or("dynamic application shebang range is invalid")?,
    )
    .map_err(|_| "dynamic application shebang is not UTF-8")?;
    let interpreter = line
        .split_ascii_whitespace()
        .next()
        .ok_or("dynamic application shebang has no interpreter")?;
    if !interpreter.starts_with('/') {
        return Err("dynamic application shebang interpreter is not absolute".into());
    }
    let resolved = match namespace.resolve(interpreter)? {
        ResolveOutcome::Found(resolved) => resolved,
        ResolveOutcome::Missing(_) => {
            return Err(format!(
                "dynamic application shebang interpreter {interpreter:?} is absent"
            ));
        }
    };
    executable_file(&resolved, "dynamic application shebang interpreter")?;
    Ok(Some(resolved))
}

fn expand_run_path(origin: &str, value: &str) -> Result<Option<String>, String> {
    let expanded = value
        .replace("${ORIGIN}", origin)
        .replace("$ORIGIN", origin);
    if !expanded.starts_with('/') {
        // Relative loader paths stay inside application-owned cwd/state. They
        // are not trusted as providers for admission; every edge must also be
        // satisfiable by one authenticated absolute package/runtime path.
        return Ok(None);
    }
    if expanded.contains('$') {
        return Err(format!(
            "dynamic application loader path {value:?} uses an unsupported token"
        ));
    }
    if expanded.len() > MAX_NAMESPACE_PATH_BYTES {
        return Err(format!(
            "dynamic application loader path exceeds {MAX_NAMESPACE_PATH_BYTES} bytes"
        ));
    }
    pending_components(&expanded)?;
    Ok(Some(expanded))
}

fn resolve_needed(
    namespace: &Namespace<'_>,
    object: &ResolvedPath,
    search_paths: &[String],
    allowed_roots: &[String],
    needed: &str,
    edges: &mut usize,
) -> Result<ResolvedPath, String> {
    if needed.is_empty() || needed.as_bytes().contains(&0) {
        return Err(format!(
            "dynamic application object {} has an empty or NUL-bearing needed entry",
            object.virtual_path
        ));
    }
    if needed.contains('$') {
        return Err(format!(
            "dynamic application object {} has unsupported token-bearing needed entry {needed:?}",
            object.virtual_path
        ));
    }
    if needed.contains('/') {
        if !needed.starts_with('/') {
            return Err(format!(
                "dynamic application object {} has ambient relative needed path {needed:?}",
                object.virtual_path
            ));
        }
        account_loader_edge(edges)?;
        let resolved = match namespace.resolve(needed)? {
            ResolveOutcome::Found(resolved) => resolved,
            ResolveOutcome::Missing(_) => {
                return Err(format!(
                    "dynamic application object {} has unresolved needed path {needed:?}",
                    object.virtual_path
                ));
            }
        };
        return require_loader_provider(resolved, allowed_roots);
    }
    for directory in search_paths {
        account_loader_edge(edges)?;
        let candidate = format!("{directory}/{needed}");
        if let ResolveOutcome::Found(resolved) = namespace.resolve(&candidate)? {
            return require_loader_provider(resolved, allowed_roots);
        }
    }
    Err(format!(
        "dynamic application object {} cannot resolve needed library {needed:?}",
        object.virtual_path
    ))
}

fn loader_search_paths(
    object: &ResolvedPath,
    run_paths: &[String],
    run_path_kind: crate::elf::RuntimeRunPathKind,
    inherited_rpaths: &Arc<Vec<String>>,
    library_paths: &[String],
) -> Result<(Vec<String>, Arc<Vec<String>>), String> {
    let origin = object
        .virtual_path
        .rsplit_once('/')
        .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
        .ok_or("dynamic loader object has no parent")?;
    let mut expanded_run_paths = Vec::new();
    for run_path in run_paths {
        if let Some(path) = expand_run_path(origin, run_path)? {
            expanded_run_paths.push(path);
        }
    }
    let mut ordered = Vec::new();
    let mut child_rpaths = Arc::clone(inherited_rpaths);
    match run_path_kind {
        crate::elf::RuntimeRunPathKind::None => {
            ordered.extend(inherited_rpaths.iter().cloned());
            ordered.extend(library_paths.iter().cloned());
        }
        crate::elf::RuntimeRunPathKind::Rpath => {
            let mut next = expanded_run_paths.clone();
            next.extend(inherited_rpaths.iter().cloned());
            let mut seen = BTreeSet::new();
            next.retain(|path| seen.insert(path.clone()));
            if next.len() > MAX_RPATH_ANCESTORS {
                return Err(format!(
                    "dynamic application RPATH ancestry exceeds {MAX_RPATH_ANCESTORS} directories"
                ));
            }
            let ancestry_bytes = next.iter().try_fold(0usize, |total, path| {
                total
                    .checked_add(path.len())
                    .ok_or("dynamic application RPATH ancestry size overflows")
            })?;
            if ancestry_bytes > MAX_RPATH_ANCESTRY_BYTES {
                return Err(format!(
                    "dynamic application RPATH ancestry exceeds {MAX_RPATH_ANCESTRY_BYTES} bytes"
                ));
            }
            child_rpaths = Arc::new(next);
            ordered.extend(child_rpaths.iter().cloned());
            ordered.extend(library_paths.iter().cloned());
        }
        crate::elf::RuntimeRunPathKind::Runpath => {
            // glibc ignores every inherited RPATH when the object causing the
            // lookup has RUNPATH, then searches its own RUNPATH after the
            // environment. RUNPATH itself does not propagate to children.
            ordered.extend(library_paths.iter().cloned());
            ordered.extend(expanded_run_paths);
        }
    }
    ordered.extend(RUNTIME_LIBRARY_PATHS.iter().map(|path| (*path).to_string()));
    let mut seen = BTreeSet::new();
    ordered.retain(|path| seen.insert(path.clone()));
    Ok((ordered, child_rpaths))
}

fn require_loader_provider(
    resolved: ResolvedPath,
    allowed_roots: &[String],
) -> Result<ResolvedPath, String> {
    if !allowed_roots
        .iter()
        .any(|root| is_same_or_child(&resolved.virtual_path, root))
    {
        return Err(format!(
            "dynamic loader provider {} is outside the reviewed library roots",
            resolved.virtual_path
        ));
    }
    let metadata = fs::metadata(&resolved.physical_path).map_err(|error| {
        format!(
            "inspect dynamic loader provider {}: {error}",
            resolved.physical_path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "dynamic loader provider {} is not a regular file",
            resolved.physical_path.display()
        ));
    }
    if metadata.permissions().mode() & 0o004 == 0 {
        return Err(format!(
            "dynamic loader provider {} is not world-readable",
            resolved.physical_path.display()
        ));
    }
    Ok(resolved)
}

fn prepare_library_roots(
    namespace: &Namespace<'_>,
    library_paths: &[String],
) -> Result<Vec<String>, String> {
    let mut roots = BTreeSet::new();
    for (path, required) in library_paths
        .iter()
        .map(|path| (path.as_str(), true))
        .chain(RUNTIME_LIBRARY_PATHS.iter().map(|path| (*path, false)))
    {
        let resolved = match namespace.resolve(path)? {
            ResolveOutcome::Found(resolved) => resolved,
            ResolveOutcome::Missing(_) if !required => continue,
            ResolveOutcome::Missing(_) => {
                return Err(format!(
                    "reviewed dynamic application library root {path:?} is absent"
                ));
            }
        };
        if required && resolved.virtual_path != path {
            return Err(format!(
                "reviewed dynamic application library root {path:?} resolves to {:?}",
                resolved.virtual_path
            ));
        }
        let metadata = fs::metadata(&resolved.physical_path).map_err(|error| {
            format!(
                "inspect dynamic application library root {}: {error}",
                resolved.physical_path.display()
            )
        })?;
        if !metadata.file_type().is_dir() {
            return Err(format!(
                "dynamic application library root {} is not a directory",
                resolved.physical_path.display()
            ));
        }
        roots.insert(resolved.virtual_path);
    }
    Ok(roots.into_iter().collect())
}

fn resolve_elf_interpreter(
    namespace: &Namespace<'_>,
    object: &ResolvedPath,
    interpreter: &str,
    allowed_roots: &[String],
) -> Result<ResolvedPath, String> {
    if !interpreter.starts_with('/') {
        return Err(format!(
            "dynamic application object {} has non-absolute interpreter {interpreter:?}",
            object.virtual_path
        ));
    }
    let resolved = match namespace.resolve(interpreter)? {
        ResolveOutcome::Found(resolved) => resolved,
        ResolveOutcome::Missing(_) => {
            return Err(format!(
                "dynamic application object {} has unresolved interpreter {interpreter:?}",
                object.virtual_path
            ));
        }
    };
    let resolved = require_loader_provider(resolved, allowed_roots)?;
    executable_file(&resolved, "dynamic application interpreter")?;
    Ok(resolved)
}

fn validate_loader_graph(
    namespace: &Namespace<'_>,
    application_files: Vec<(String, PathBuf)>,
    entry_interpreter: Option<ResolvedPath>,
    library_paths: &[String],
    allowed_roots: &[String],
) -> Result<(), String> {
    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    enum RequiredElf {
        None,
        Executable,
        SharedObject,
    }

    #[derive(Debug)]
    struct PendingObject {
        object: ResolvedPath,
        required_elf: RequiredElf,
        inherited_rpaths: Arc<Vec<String>>,
    }

    let mut pending = VecDeque::new();
    for (virtual_path, physical_path) in application_files {
        pending.push_back(PendingObject {
            object: ResolvedPath {
                virtual_path,
                physical_path,
            },
            required_elf: RequiredElf::None,
            inherited_rpaths: Arc::new(Vec::new()),
        });
    }
    if let Some(interpreter) = entry_interpreter {
        pending.push_back(PendingObject {
            object: interpreter,
            required_elf: RequiredElf::Executable,
            inherited_rpaths: Arc::new(Vec::new()),
        });
    }
    let mut visited = std::collections::BTreeMap::new();
    let mut parsed = std::collections::BTreeMap::new();
    let mut unique_objects = BTreeSet::new();
    let mut edges = 0usize;
    let mut retained_state_bytes = 0usize;
    let mut loader_file_bytes = 0u64;
    while let Some(PendingObject {
        object,
        required_elf,
        inherited_rpaths,
    }) = pending.pop_front()
    {
        let state = (object.virtual_path.clone(), Arc::clone(&inherited_rpaths));
        match visited.get(&state) {
            Some(previous) if *previous >= required_elf => continue,
            Some(_) => {
                visited.insert(state, required_elf);
            }
            None => {
                account_loader_state(
                    &mut retained_state_bytes,
                    &object.virtual_path,
                    &inherited_rpaths,
                )?;
                visited.insert(state, required_elf);
            }
        }
        unique_objects.insert(object.virtual_path.clone());
        if unique_objects.len() > MAX_LOADER_OBJECTS {
            return Err(format!(
                "dynamic application loader graph exceeds {MAX_LOADER_OBJECTS} objects"
            ));
        }
        let max_states = MAX_LOADER_OBJECTS
            .checked_add(MAX_LOADER_EDGES)
            .ok_or("dynamic application loader-state limit overflows")?;
        if visited.len() > max_states {
            return Err(format!(
                "dynamic application loader graph exceeds {max_states} ancestry states"
            ));
        }
        let search = match parsed.get(&object.virtual_path) {
            Some(search) => Arc::clone(search),
            None => {
                let reserved_bytes = fs::metadata(&object.physical_path)
                    .map_err(|error| {
                        format!(
                            "stat dynamic application object {}: {error}",
                            object.physical_path.display()
                        )
                    })?
                    .len();
                account_loader_file_bytes(&mut loader_file_bytes, reserved_bytes)?;
                let search = Arc::new(
                    crate::elf::runtime_link_search_bounded(
                        &object.physical_path,
                        crate::elf::RuntimeLinkLimits {
                            file_bytes: MAX_DYNAMIC_ELF_BYTES,
                            dynamic_entries: MAX_LOADER_EDGES,
                            references: MAX_LOADER_EDGES,
                            string_bytes: MAX_DYNAMIC_STRING_BYTES,
                            aggregate_text_bytes: MAX_DYNAMIC_TEXT_BYTES,
                        },
                    )
                    .map_err(|error| {
                        format!(
                            "inspect dynamic application object {}: {error}",
                            object.physical_path.display()
                        )
                    })?,
                );
                if search.file_bytes > reserved_bytes {
                    account_loader_file_bytes(
                        &mut loader_file_bytes,
                        search.file_bytes - reserved_bytes,
                    )?;
                }
                parsed.insert(object.virtual_path.clone(), Arc::clone(&search));
                search
            }
        };
        let required_kind = match required_elf {
            RequiredElf::None => None,
            RequiredElf::Executable => Some(search.executable),
            RequiredElf::SharedObject => Some(matches!(
                search.kind,
                crate::elf::RuntimeElfKind::SharedObject
            )),
        };
        if required_kind == Some(false) {
            return Err(format!(
                "dynamic loader provider {} has the wrong ELF role ({:?})",
                object.virtual_path, search.kind
            ));
        }
        if let Some(interpreter) = &search.interpreter {
            account_loader_edge(&mut edges)?;
            let resolved =
                resolve_elf_interpreter(namespace, &object, &interpreter, allowed_roots)?;
            pending.push_back(PendingObject {
                object: resolved,
                required_elf: RequiredElf::Executable,
                inherited_rpaths: Arc::new(Vec::new()),
            });
        }
        let (search_paths, child_rpaths) = loader_search_paths(
            &object,
            &search.run_paths,
            search.run_path_kind,
            &inherited_rpaths,
            library_paths,
        )?;
        for dependency in &search.needed {
            pending.push_back(PendingObject {
                object: resolve_needed(
                    namespace,
                    &object,
                    &search_paths,
                    allowed_roots,
                    dependency,
                    &mut edges,
                )?,
                required_elf: RequiredElf::SharedObject,
                inherited_rpaths: Arc::clone(&child_rpaths),
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_dynamic_application(
    out: &Path,
    entry: &str,
    runtime: &Path,
    library_paths: &[String],
    optional_targets: &[String],
    expected_optional_links: usize,
) -> Result<(), String> {
    validate_library_paths(library_paths)?;
    validate_optional_roots(optional_targets)?;
    let app = out.join("files");
    let usr = runtime.join("files");
    validate_tree_root(&app, "dynamic application files root")?;
    validate_tree_root(&usr, "dynamic application runtime files root")?;
    let namespace = Namespace {
        app: &app,
        usr: &usr,
    };
    validate_runtime_alias_targets(&namespace)?;
    let entry_interpreter = validate_entry(&namespace, entry)?;
    let allowed_roots = prepare_library_roots(&namespace, library_paths)?;
    let (files, optional_links) = collect_application_files(&namespace, optional_targets)?;
    if optional_links != expected_optional_links {
        return Err(format!(
            "dynamic application has {optional_links} omitted-extension links, expected \
             {expected_optional_links}"
        ));
    }
    validate_loader_graph(
        &namespace,
        files,
        entry_interpreter,
        library_paths,
        &allowed_roots,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "td-dynamic-application-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn make_executable(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn aliases_and_cross_tree_links_resolve_without_host_paths() {
        let root = scratch("aliases");
        let app = root.join("app");
        let usr = root.join("usr");
        fs::create_dir_all(app.join("lib")).unwrap();
        fs::create_dir_all(usr.join("lib64")).unwrap();
        fs::write(usr.join("lib64/loader"), b"loader").unwrap();
        symlink("/lib64/loader", app.join("lib/loader")).unwrap();
        let resolved = match (Namespace {
            app: &app,
            usr: &usr,
        })
        .resolve("/app/lib/loader")
        .unwrap()
        {
            ResolveOutcome::Found(resolved) => resolved,
            ResolveOutcome::Missing(path) => panic!("missing {path}"),
        };
        assert_eq!(resolved.virtual_path, "/usr/lib64/loader");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indirect_run_host_and_escape_are_refused() {
        let root = scratch("forbidden");
        let app = root.join("app");
        let usr = root.join("usr");
        fs::create_dir_all(app.join("lib")).unwrap();
        fs::create_dir_all(usr.join("lib")).unwrap();
        symlink("/usr/lib/second", app.join("lib/first")).unwrap();
        symlink("/run/host/lib", usr.join("lib/second")).unwrap();
        let error = Namespace {
            app: &app,
            usr: &usr,
        }
        .resolve("/app/lib/first")
        .unwrap_err();
        assert!(
            error.contains("outside") || error.contains("/run/host"),
            "{error}"
        );
        fs::create_dir_all(usr.join("safe")).unwrap();
        symlink("/run/host/dir", usr.join("escape")).unwrap();
        let error = Namespace {
            app: &app,
            usr: &usr,
        }
        .resolve("/usr/escape/../safe")
        .unwrap_err();
        assert!(
            error.contains("outside") || error.contains("/run/host"),
            "{error}"
        );

        let namespace = Namespace {
            app: &app,
            usr: &usr,
        };
        let object = ResolvedPath {
            virtual_path: "/app/lib/object.so".into(),
            physical_path: app.join("lib/object.so"),
        };
        let expanded = expand_run_path("/app/lib", "/usr/escape/../safe")
            .unwrap()
            .unwrap();
        assert_eq!(expanded, "/usr/escape/../safe");
        let mut edges = 0;
        let error = resolve_needed(
            &namespace,
            &object,
            &[expanded],
            &["/app/lib".into(), "/usr/lib".into()],
            "provider.so",
            &mut edges,
        )
        .unwrap_err();
        assert!(
            error.contains("outside") || error.contains("/run/host"),
            "{error}"
        );
        assert!(normalize_virtual("/app", "../../host").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn elf_interpreters_must_be_absolute_reviewed_executables() {
        let root = scratch("relative-interpreter");
        let app = root.join("app");
        let usr = root.join("usr");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(usr.join("lib64")).unwrap();
        let loader = usr.join("lib64/loader.so");
        fs::write(&loader, b"loader").unwrap();
        make_executable(&loader);
        let namespace = Namespace {
            app: &app,
            usr: &usr,
        };
        let object = ResolvedPath {
            virtual_path: "/app/lib/object.so".into(),
            physical_path: app.join("lib/object.so"),
        };
        let roots = vec!["/usr/lib64".into()];
        let error = resolve_elf_interpreter(&namespace, &object, "usr/lib64/loader.so", &roots)
            .unwrap_err();
        assert!(error.contains("non-absolute interpreter"), "{error}");
        assert_eq!(
            resolve_elf_interpreter(&namespace, &object, "/lib64/loader.so", &roots)
                .unwrap()
                .virtual_path,
            "/usr/lib64/loader.so"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entry_parent_links_and_wrong_architecture_do_not_bypass_admission() {
        let root = scratch("entry");
        let app = root.join("app");
        let usr = root.join("usr");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(usr.join("bin")).unwrap();
        fs::write(usr.join("bin/env"), b"#!/bin/sh\n").unwrap();
        make_executable(&usr.join("bin/env"));
        symlink("/usr/bin", app.join("tools")).unwrap();
        let namespace = Namespace {
            app: &app,
            usr: &usr,
        };
        let error = validate_entry(&namespace, "/app/tools/env").unwrap_err();
        assert!(error.contains("not a direct /app file"), "{error}");

        fs::create_dir_all(app.join("bin")).unwrap();
        let entry = app.join("bin/wrong-arch");
        let mut wrong_arch = crate::elf::tests::synth_needed_elf(&[], true);
        wrong_arch[0x12..0x14].copy_from_slice(&3u16.to_le_bytes());
        fs::write(&entry, wrong_arch).unwrap();
        make_executable(&entry);
        let error = validate_entry(&namespace, "/app/bin/wrong-arch").unwrap_err();
        assert!(error.contains("expected EM_X86_64"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn optional_targets_are_explicit_and_component_bounded() {
        assert!(validate_optional_roots(&["/app/share/runtime/langpack".into()]).is_ok());
        assert!(!is_same_or_child(
            "/app/share/runtime/langpacks/evil",
            "/app/share/runtime/langpack"
        ));
        assert!(validate_optional_roots(&["/usr/share/locale".into()]).is_err());
    }

    #[test]
    fn relative_runpaths_are_not_admission_providers() {
        assert_eq!(expand_run_path("/app/lib", "$").unwrap(), None);
        assert_eq!(
            expand_run_path("/app/lib/firefox", "$ORIGIN").unwrap(),
            Some("/app/lib/firefox".into())
        );
        assert!(expand_run_path("/app/lib", "/usr/$LIB").is_err());
    }

    #[test]
    fn rpath_and_runpath_take_their_real_environment_precedence() {
        let object = ResolvedPath {
            virtual_path: "/app/lib/firefox/object.so".into(),
            physical_path: PathBuf::from("unused"),
        };
        let run_paths = vec!["/app/lib/firefox".into()];
        let environment = vec!["/app/lib".into()];
        let inherited = Arc::new(vec!["/app/ancestor".into()]);
        let (paths, child_rpaths) = loader_search_paths(
            &object,
            &run_paths,
            crate::elf::RuntimeRunPathKind::Rpath,
            &inherited,
            &environment,
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                "/app/lib/firefox",
                "/app/ancestor",
                "/app/lib",
                "/usr/lib/x86_64-linux-gnu",
                "/usr/lib64",
                "/usr/lib"
            ]
        );
        assert_eq!(
            child_rpaths.as_slice(),
            ["/app/lib/firefox", "/app/ancestor"]
        );

        let (paths, child_rpaths) = loader_search_paths(
            &object,
            &run_paths,
            crate::elf::RuntimeRunPathKind::Runpath,
            &inherited,
            &environment,
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                "/app/lib",
                "/app/lib/firefox",
                "/usr/lib/x86_64-linux-gnu",
                "/usr/lib64",
                "/usr/lib"
            ]
        );
        assert_eq!(child_rpaths.as_slice(), ["/app/ancestor"]);
    }

    #[test]
    fn every_loader_edge_kind_uses_one_exact_budget() {
        let mut edges = MAX_LOADER_EDGES;
        let error = account_loader_edge(&mut edges).unwrap_err();
        assert!(error.contains("loader graph"), "{error}");

        let mut retained_bytes = MAX_LOADER_STATE_BYTES;
        let error = account_loader_state(&mut retained_bytes, "/app/object.so", &[]).unwrap_err();
        assert!(error.contains("retained bytes"), "{error}");
        let mut file_bytes = MAX_LOADER_FILE_BYTES;
        let error = account_loader_file_bytes(&mut file_bytes, 1).unwrap_err();
        assert!(error.contains("aggregate bytes"), "{error}");

        let namespace = Namespace {
            app: Path::new("/absent-app"),
            usr: Path::new("/absent-usr"),
        };
        let object = ResolvedPath {
            virtual_path: "/app/object.so".into(),
            physical_path: PathBuf::from("unused"),
        };
        for needed in ["lib$LIB.so", "${ORIGIN}/lib.so"] {
            let mut edges = 0;
            let error = resolve_needed(
                &namespace,
                &object,
                &["/app/lib".into()],
                &["/app/lib".into()],
                needed,
                &mut edges,
            )
            .unwrap_err();
            assert!(error.contains("token-bearing"), "{error}");
        }
        let mut edges = MAX_LOADER_EDGES;
        let error = resolve_needed(
            &namespace,
            &object,
            &["/app/lib".into()],
            &["/app/lib".into()],
            "provider.so",
            &mut edges,
        )
        .unwrap_err();
        assert!(error.contains("loader graph"), "{error}");
    }

    #[test]
    fn providers_stay_inside_reviewed_roots_after_symlink_resolution() {
        let root = scratch("providers");
        let app = root.join("app");
        let usr = root.join("usr");
        fs::create_dir_all(app.join("lib")).unwrap();
        fs::create_dir_all(app.join("share")).unwrap();
        fs::create_dir_all(usr.join("lib")).unwrap();
        fs::create_dir_all(usr.join("bin")).unwrap();
        fs::write(app.join("share/payload.so"), b"not elf").unwrap();
        fs::write(usr.join("lib/libok.so"), b"not elf").unwrap();
        fs::write(usr.join("bin/tool"), b"not elf").unwrap();
        symlink("/usr/lib/libok.so", app.join("lib/libok.so")).unwrap();
        symlink("/usr/bin/tool", app.join("lib/escape.so")).unwrap();
        let namespace = Namespace {
            app: &app,
            usr: &usr,
        };
        let library_paths = vec!["/app/lib".to_string()];
        let allowed = prepare_library_roots(&namespace, &library_paths).unwrap();
        let object = ResolvedPath {
            virtual_path: "/app/bin/object".into(),
            physical_path: app.join("bin/object"),
        };
        let mut edges = 0;
        assert_eq!(
            resolve_needed(
                &namespace,
                &object,
                &library_paths,
                &allowed,
                "libok.so",
                &mut edges,
            )
            .unwrap()
            .virtual_path,
            "/usr/lib/libok.so"
        );
        for needed in ["escape.so", "/app/share/payload.so"] {
            let error = resolve_needed(
                &namespace,
                &object,
                &library_paths,
                &allowed,
                needed,
                &mut edges,
            )
            .unwrap_err();
            assert!(
                error.contains("outside the reviewed library roots"),
                "{error}"
            );
        }
        fs::set_permissions(usr.join("lib/libok.so"), fs::Permissions::from_mode(0o600)).unwrap();
        let error = resolve_needed(
            &namespace,
            &object,
            &library_paths,
            &allowed,
            "libok.so",
            &mut edges,
        )
        .unwrap_err();
        assert!(error.contains("not world-readable"), "{error}");
        fs::set_permissions(usr.join("lib/libok.so"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(app.join("lib/libok.so")).unwrap();
        fs::remove_file(app.join("lib/escape.so")).unwrap();
        fs::remove_dir(app.join("lib")).unwrap();
        symlink("/app", app.join("lib")).unwrap();
        let error = prepare_library_roots(&namespace, &library_paths).unwrap_err();
        assert!(error.contains("resolves to"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_validator_closes_the_shebang_interpreter_graph() {
        let root = scratch("shebang-graph");
        let package = root.join("package");
        let runtime = root.join("runtime");
        let app = package.join("files");
        let usr = runtime.join("files");
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::create_dir_all(app.join("lib/firefox")).unwrap();
        fs::create_dir_all(usr.join("bin")).unwrap();
        fs::create_dir_all(usr.join("lib")).unwrap();
        fs::create_dir_all(usr.join("lib64")).unwrap();
        fs::create_dir_all(usr.join("sbin")).unwrap();
        let entry = app.join("bin/firefox");
        fs::write(&entry, b"#!/bin/bash\nexit 0\n").unwrap();
        make_executable(&entry);
        let bash = usr.join("bin/bash");
        fs::write(
            &bash,
            crate::elf::tests::synth_needed_elf(&["libmissing.so"], true),
        )
        .unwrap();
        make_executable(&bash);
        let paths = vec!["/app/lib".into(), "/app/lib/firefox".into()];
        let error =
            validate_dynamic_application(&package, "/app/bin/firefox", &runtime, &paths, &[], 0)
                .unwrap_err();
        assert!(error.contains("libmissing.so"), "{error}");
        let mut wrong_role = crate::elf::tests::synth_needed_elf(&[], true);
        wrong_role[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        fs::write(usr.join("lib/libmissing.so"), wrong_role).unwrap();
        let error =
            validate_dynamic_application(&package, "/app/bin/firefox", &runtime, &paths, &[], 0)
                .unwrap_err();
        assert!(error.contains("wrong ELF role"), "{error}");
        fs::write(
            usr.join("lib/libmissing.so"),
            crate::elf::tests::synth_needed_elf(&[], true),
        )
        .unwrap();
        validate_dynamic_application(&package, "/app/bin/firefox", &runtime, &paths, &[], 0)
            .unwrap();
        fs::remove_dir(usr.join("sbin")).unwrap();
        let error =
            validate_dynamic_application(&package, "/app/bin/firefox", &runtime, &paths, &[], 0)
                .unwrap_err();
        assert!(error.contains("alias /sbin"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires the separately warmed and materialized exact Flathub deploys"]
    fn exact_firefox_and_platform_deploys_pass_offline_admission() {
        let package = PathBuf::from(std::env::var_os("TD_FIREFOX_PACKAGE").unwrap());
        let runtime = PathBuf::from(std::env::var_os("TD_FIREFOX_RUNTIME").unwrap());
        validate_dynamic_application(
            &package,
            "/app/bin/firefox",
            &runtime,
            &["/app/lib".into(), "/app/lib/firefox".into()],
            &["/app/share/runtime/langpack".into()],
            102,
        )
        .unwrap();
    }
}
