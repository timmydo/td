use crate::json;
use crate::symbol;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const HEADER: &str = "td-profiler-objects-v1";
const MAX_ENTRIES: usize = 200_000;
const MAX_WALK_PATH_BYTES: usize = 64 * 1024 * 1024;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_EXCLUSION_TABLE_BYTES: u64 = 1024 * 1024;
const APPLICATION_ROOTS_HEADER: &str = "td-profiler-application-roots-v1";
const MAX_APPLICATION_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_APPLICATION_SPEC_BYTES: u64 = 48 * 1024;
const MAX_ASSEMBLY_MARKER_BYTES: u64 = 64 * 1024;
const MAX_LINE_ATTRIBUTION_MARKER_BYTES: u64 = 64 * 1024;

pub fn registry_exclusions(
    root: &Path,
    path: &Path,
    roots_path: &Path,
) -> Result<Vec<PathBuf>, String> {
    if !root.is_absolute() || !root.is_dir() {
        return Err(format!(
            "application-registry root must be an absolute directory: {}",
            root.display()
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("stat application registry {}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_EXCLUSION_TABLE_BYTES {
        return Err(format!(
            "application registry must be a bounded regular file: {}",
            path.display()
        ));
    }
    let text = read_bounded_text(path, MAX_EXCLUSION_TABLE_BYTES, "application registry")?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("application registry lacks its canonical trailing newline".into());
    }
    let mut policies = application_root_policies(roots_path)?;
    let mut prior = None;
    let mut source_roots = BTreeSet::new();
    let mut foreign_roots = BTreeSet::new();
    for (number, line) in text.lines().enumerate() {
        let Some((name, path)) = line.split_once('\t') else {
            return Err(format!(
                "application registry row {} is not canonical name-tab-path",
                number + 1
            ));
        };
        if path.contains('\t') || text_field(name).is_err() {
            return Err(format!(
                "application registry row {} is not canonical name-tab-path",
                number + 1
            ));
        }
        if prior.is_some_and(|value: &str| value >= name) {
            return Err("application registry names are not strictly sorted".into());
        }
        prior = Some(name);
        let package = PathBuf::from(path);
        if !store_root(&package) {
            return Err(format!(
                "application registry row {} has a non-store package path",
                number + 1
            ));
        }
        let package_fs = image_path(root, &package)?;
        let manifest = read_metadata(
            &package_fs.join("manifest"),
            MAX_APPLICATION_MANIFEST_BYTES,
            "application manifest",
        )?;
        let manifest = application_manifest(&manifest, name)?;
        let spec = read_metadata(
            &package_fs.join("spec"),
            MAX_APPLICATION_SPEC_BYTES,
            "application spec",
        )?;
        let runtime = application_runtime(&spec, name)?;
        let policy = policies
            .remove(name)
            .ok_or_else(|| format!("application root policy has no row for {name:?}"))?;
        if manifest.identity != policy.package_identity
            || !store_identity(&package, &policy.package_identity)
        {
            return Err(format!(
                "application package identity disagrees with root policy for {name:?}"
            ));
        }
        if !store_identity(&runtime, &policy.runtime_identity) {
            return Err(format!(
                "application runtime identity disagrees with root policy for {name:?}"
            ));
        }
        if !policy
            .runtime_identity
            .strip_prefix(&manifest.runtime)
            .is_some_and(|version| version.starts_with('-') && version.len() > 1)
        {
            return Err(format!(
                "application runtime recipe disagrees with manifest for {name:?}"
            ));
        }
        let roots = match policy.package_provenance {
            Provenance::Source => &mut source_roots,
            Provenance::Foreign => &mut foreign_roots,
        };
        roots.insert(package);
        let roots = match policy.runtime_provenance {
            Provenance::Source => &mut source_roots,
            Provenance::Foreign => &mut foreign_roots,
        };
        roots.insert(runtime);
    }
    if !policies.is_empty() {
        return Err("application root policy has rows absent from the registry".into());
    }
    Ok(foreign_roots.difference(&source_roots).cloned().collect())
}

struct ApplicationRootPolicy {
    package_identity: String,
    package_provenance: Provenance,
    runtime_identity: String,
    runtime_provenance: Provenance,
}

fn application_root_policies(
    path: &Path,
) -> Result<BTreeMap<String, ApplicationRootPolicy>, String> {
    let text = read_metadata(path, MAX_EXCLUSION_TABLE_BYTES, "application root policy")?;
    if !text.ends_with('\n') {
        return Err("application root policy lacks its canonical trailing newline".into());
    }
    let mut lines = text.lines();
    if lines.next() != Some(APPLICATION_ROOTS_HEADER) {
        return Err("application root policy has the wrong header".into());
    }
    let mut policies = BTreeMap::new();
    let mut prior = None;
    for (number, line) in lines.enumerate() {
        let fields: Vec<_> = line.split('\t').collect();
        let [name, package_identity, package_provenance, runtime_identity, runtime_provenance] =
            fields.as_slice()
        else {
            return Err(format!(
                "application root policy row {} is not canonical",
                number + 2
            ));
        };
        for field in [
            *name,
            *package_identity,
            *package_provenance,
            *runtime_identity,
            *runtime_provenance,
        ] {
            text_field(field)?;
        }
        if prior.is_some_and(|value: &str| value >= *name) {
            return Err("application root policy names are not strictly sorted".into());
        }
        prior = Some(name);
        policies.insert(
            (*name).to_string(),
            ApplicationRootPolicy {
                package_identity: (*package_identity).to_string(),
                package_provenance: provenance(package_provenance)?,
                runtime_identity: (*runtime_identity).to_string(),
                runtime_provenance: provenance(runtime_provenance)?,
            },
        );
    }
    Ok(policies)
}

fn provenance(value: &str) -> Result<Provenance, String> {
    match value {
        "source" => Ok(Provenance::Source),
        "foreign" => Ok(Provenance::Foreign),
        _ => Err(format!("invalid application root provenance {value:?}")),
    }
}

fn store_identity(path: &Path, expected: &str) -> bool {
    path.file_name()
        .and_then(|item| item.to_str())
        .and_then(|item| item.split_once('-'))
        .is_some_and(|(_, identity)| identity == expected)
}

pub fn build(root: &Path, output: &Path, exclusions: &[PathBuf]) -> Result<(), String> {
    if !root.is_absolute() || !root.is_dir() {
        return Err(format!(
            "object-index root must be an absolute directory: {}",
            root.display()
        ));
    }
    let mut normalized = Vec::with_capacity(exclusions.len());
    for exclusion in exclusions {
        if !store_root(exclusion) {
            return Err(format!(
                "object-index exclusion is not one canonical /td/store child: {}",
                exclusion.display()
            ));
        }
        normalized.push(exclusion.clone());
    }
    normalized.sort();
    normalized.dedup();

    let mut runtimes = Vec::new();
    let mut debugs = Vec::new();
    let mut visited = 0usize;
    let mut visited_path_bytes = 0usize;
    walk(
        root,
        root,
        &normalized,
        &mut visited,
        &mut visited_path_bytes,
        &mut runtimes,
        &mut debugs,
    )?;
    runtimes.sort();
    debugs.sort();
    if runtimes.is_empty() {
        return Err("object index found no profiled runtime ELF".into());
    }

    let mut debug_by_path = BTreeMap::new();
    let mut debug_by_id: BTreeMap<Vec<u8>, Vec<PathBuf>> = BTreeMap::new();
    for (debug_fs, debug) in debugs {
        let id = symbol::load_build_id(&debug_fs)?;
        let item = store_item(&debug)?;
        debug_by_id
            .entry(id.clone())
            .or_default()
            .push(debug.clone());
        if debug_by_path.insert(debug, (debug_fs, item, id)).is_some() {
            return Err("object index found a duplicate debug path".into());
        }
    }

    let mut rows = Vec::with_capacity(runtimes.len());
    let mut row_bytes = HEADER.len().saturating_add(2);
    let mut assembly = BTreeMap::new();
    let mut line_attribution = BTreeMap::new();
    for (runtime_fs, runtime) in runtimes {
        let runtime_id = symbol::load_build_id(&runtime_fs)?;
        let (expected_debug, runtime_item, runtime_within) = debug_path(&runtime)?;
        let debug = if debug_by_path.contains_key(&expected_debug) {
            expected_debug
        } else {
            debug_by_id
                .get(&runtime_id)
                .and_then(|paths| paths.first())
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "profiled runtime has no build-ID-matched debug companion: {}",
                        runtime.display()
                    )
                })?
        };
        let (_, debug_item, debug_id) = debug_by_path
            .get(&debug)
            .ok_or("internal: selected debug companion disappeared")?;
        if runtime_id.as_slice() != debug_id.as_slice() {
            return Err(format!(
                "runtime/debug build IDs disagree: {} and {}",
                runtime.display(),
                debug.display()
            ));
        }
        let line_boundary =
            line_attribution_boundary(root, &runtime_item, &runtime_within, &mut line_attribution)?;
        let runtime = field(&runtime)?;
        let debug = field(&debug)?;
        let mut provenance = format!("store-item={runtime_item}");
        if debug_item != &runtime_item {
            provenance.push_str(&format!(";debug-store-item={debug_item}"));
        }
        if assembly_boundary(root, debug_item, &mut assembly)? {
            provenance.push_str(";assembly-boundary=1");
        }
        if line_boundary {
            provenance.push_str(";line-attribution-boundary=1");
        }
        let provenance = text_field(&provenance)?;
        let row = format!(
            "{runtime}\t{debug}\t{}\t{provenance}",
            json::hex(&runtime_id)
        );
        row_bytes = row_bytes
            .checked_add(row.len().saturating_add(1))
            .ok_or("object index byte count overflow")?;
        if row_bytes > MAX_BYTES {
            return Err(format!("object index exceeds {MAX_BYTES} bytes"));
        }
        rows.push(row);
    }
    rows.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if rows.windows(2).any(|pair| pair.first() == pair.get(1)) {
        return Err("object index produced a duplicate row".into());
    }
    let parent = output
        .parent()
        .ok_or_else(|| format!("object index output has no parent: {}", output.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("create object index parent {}: {e}", parent.display()))?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(output)
        .map_err(|e| format!("create object index {}: {e}", output.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("chmod object index {}: {e}", output.display()))?;
    let mut file = BufWriter::new(file);
    file.write_all(HEADER.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| {
            for row in rows {
                file.write_all(row.as_bytes())?;
                file.write_all(b"\n")?;
            }
            Ok(())
        })
        .and_then(|()| file.flush())
        .map_err(|e| format!("write object index {}: {e}", output.display()))?;
    file.get_ref()
        .sync_all()
        .map_err(|e| format!("fsync object index {}: {e}", output.display()))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| format!("fsync object index parent {}: {e}", parent.display()))
}

fn walk(
    root: &Path,
    directory: &Path,
    exclusions: &[PathBuf],
    visited: &mut usize,
    visited_path_bytes: &mut usize,
    runtimes: &mut Vec<(PathBuf, PathBuf)>,
    debugs: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|e| format!("read image directory {}: {e}", directory.display()))?
    {
        *visited = visited.checked_add(1).ok_or("image entry count overflow")?;
        if *visited > MAX_ENTRIES {
            return Err(format!("image walk exceeds {MAX_ENTRIES} entries"));
        }
        let entry =
            entry.map_err(|e| format!("read image directory {}: {e}", directory.display()))?;
        *visited_path_bytes = visited_path_bytes
            .checked_add(entry.path().as_os_str().as_bytes().len())
            .ok_or("image path-byte count overflow")?;
        if *visited_path_bytes > MAX_WALK_PATH_BYTES {
            return Err(format!(
                "image walk exceeds its {MAX_WALK_PATH_BYTES}-byte path budget"
            ));
        }
        entries.push(entry);
    }
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let target = target_path(root, &path)?;
        if exclusions
            .iter()
            .any(|exclusion| target.starts_with(exclusion))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("stat image entry {}: {e}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            walk(
                root,
                &path,
                exclusions,
                visited,
                visited_path_bytes,
                runtimes,
                debugs,
            )?;
        } else if metadata.is_file()
            && target.extension().and_then(|extension| extension.to_str()) == Some("debug")
            && inside_debug_tree(&target)
            && symbol::is_runtime_elf(&path)?
        {
            if debugs.len() >= MAX_ENTRIES {
                return Err(format!("debug inventory exceeds {MAX_ENTRIES} entries"));
            }
            debugs.push((path, target));
        } else if metadata.is_file()
            && !inside_debug_tree(&target)
            && symbol::is_runtime_elf(&path)?
        {
            if runtimes.len() >= MAX_ENTRIES {
                return Err(format!("object index exceeds {MAX_ENTRIES} entries"));
            }
            runtimes.push((path, target));
        }
    }
    Ok(())
}

fn target_path(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let relative = path.strip_prefix(root).map_err(|_| {
        format!(
            "image entry {} escaped root {}",
            path.display(),
            root.display()
        )
    })?;
    Ok(Path::new("/").join(relative))
}

fn inside_debug_tree(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    components.windows(2).any(|pair| {
        matches!(pair.first(), Some(Component::Normal(value)) if *value == "lib")
            && matches!(pair.get(1), Some(Component::Normal(value)) if *value == "debug")
    })
}

fn debug_path(runtime: &Path) -> Result<(PathBuf, String, PathBuf), String> {
    if !store_path(runtime) {
        return Err(format!(
            "profiled runtime is not one canonical /td/store child: {}",
            runtime.display()
        ));
    }
    let relative = runtime
        .strip_prefix("/td/store")
        .map_err(|_| "runtime escaped /td/store")?;
    let mut components = relative.components();
    let item = match components.next() {
        Some(Component::Normal(item)) => item,
        _ => return Err("runtime has no store item".into()),
    };
    let within: PathBuf = components.collect();
    if within.as_os_str().is_empty() {
        return Err("runtime is the store item directory".into());
    }
    let file = within
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("runtime filename is not UTF-8")?;
    let mut debug_within = within.clone();
    debug_within.set_file_name(format!("{file}.debug"));
    let debug = Path::new("/td/store")
        .join(item)
        .join("lib/debug")
        .join(debug_within);
    let item = item.to_str().ok_or("store item is not UTF-8")?;
    Ok((debug, item.to_string(), within))
}

fn store_item(path: &Path) -> Result<String, String> {
    if !store_path(path) {
        return Err(format!(
            "profiled object is not below one canonical /td/store child: {}",
            path.display()
        ));
    }
    path.strip_prefix("/td/store")
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| match component {
            Component::Normal(item) => item.to_str(),
            _ => None,
        })
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "profiled object has no UTF-8 store item: {}",
                path.display()
            )
        })
}

fn assembly_boundary(
    root: &Path,
    item: &str,
    cache: &mut BTreeMap<String, bool>,
) -> Result<bool, String> {
    if let Some(boundary) = cache.get(item) {
        return Ok(*boundary);
    }
    let marker = root
        .join("td/store")
        .join(item)
        .join("lib/debug/.td-assembly-exception");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            cache.insert(item.to_string(), false);
            return Ok(false);
        }
        Err(error) => {
            return Err(format!(
                "stat assembly marker {}: {error}",
                marker.display()
            ))
        }
    };
    if !metadata.is_file()
        || metadata.len() > MAX_ASSEMBLY_MARKER_BYTES
        || metadata.mode() & 0o777 != 0o644
    {
        return Err(format!(
            "assembly marker must be a bounded mode-0644 regular file: {}",
            marker.display()
        ));
    }
    let text = read_bounded_text(&marker, MAX_ASSEMBLY_MARKER_BYTES, "assembly marker")?;
    validate_assembly_marker(&text)?;
    cache.insert(item.to_string(), true);
    Ok(true)
}

fn validate_assembly_marker(text: &str) -> Result<(), String> {
    if text.is_empty() || !text.ends_with('\n') || text.contains('\r') || text.contains('\0') {
        return Err("assembly marker is not canonical text".into());
    }
    let lines: Vec<_> = text.lines().collect();
    if lines.first() != Some(&"format=1") {
        return Err("assembly marker lacks format=1".into());
    }
    let output = lines
        .get(1)
        .and_then(|line| line.strip_prefix("output="))
        .ok_or("assembly marker lacks its output")?;
    text_field(output)?;
    let exceptions = lines.get(2..).unwrap_or_default();
    if exceptions.is_empty() || exceptions.len() % 2 != 0 {
        return Err("assembly marker has no complete exception pair".into());
    }
    for index in 0..exceptions.len() / 2 {
        let source = exceptions
            .get(index.saturating_mul(2))
            .and_then(|line| line.strip_prefix(&format!("exception.{index}.source=")))
            .ok_or("assembly marker exception sources are not canonical")?;
        let reason = exceptions
            .get(index.saturating_mul(2).saturating_add(1))
            .and_then(|line| line.strip_prefix(&format!("exception.{index}.reason=")))
            .ok_or("assembly marker exception reasons are not canonical")?;
        text_field(source)?;
        text_field(reason)?;
    }
    Ok(())
}

fn line_attribution_boundary(
    root: &Path,
    item: &str,
    runtime_within: &Path,
    cache: &mut BTreeMap<String, Option<PathBuf>>,
) -> Result<bool, String> {
    let marked_runtime = if let Some(runtime) = cache.get(item) {
        runtime.clone()
    } else {
        let marker = root
            .join("td/store")
            .join(item)
            .join("lib/debug/.td-line-attribution-exception");
        let metadata = match fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                cache.insert(item.to_string(), None);
                return Ok(false);
            }
            Err(error) => {
                return Err(format!(
                    "stat line-attribution marker {}: {error}",
                    marker.display()
                ));
            }
        };
        if !metadata.is_file()
            || metadata.len() > MAX_LINE_ATTRIBUTION_MARKER_BYTES
            || metadata.mode() & 0o777 != 0o644
        {
            return Err(format!(
                "line-attribution marker must be a bounded mode-0644 regular file: {}",
                marker.display()
            ));
        }
        let text = read_bounded_text(
            &marker,
            MAX_LINE_ATTRIBUTION_MARKER_BYTES,
            "line-attribution marker",
        )?;
        let marked_runtime = validate_line_attribution_marker(&text, item)?;
        cache.insert(item.to_string(), Some(marked_runtime.clone()));
        Some(marked_runtime)
    };
    Ok(marked_runtime
        .as_ref()
        .is_some_and(|marked| marked == runtime_within))
}

fn validate_line_attribution_marker(text: &str, item: &str) -> Result<PathBuf, String> {
    const SHAPE: [&str; 7] = [
        "format",
        "output",
        "runtime",
        "reader_ceiling_bytes",
        "admitted_ceiling_bytes",
        "companion_ceiling_bytes",
        "reason",
    ];
    if text.is_empty() || !text.ends_with('\n') || text.contains('\r') || text.contains('\0') {
        return Err("line-attribution marker is not canonical text".into());
    }
    if text.lines().count() != SHAPE.len() {
        return Err("line-attribution marker is not exactly its canonical header".into());
    }
    let mut fields = BTreeMap::new();
    for (line, key) in text.lines().zip(SHAPE) {
        let prefix = format!("{key}=");
        let value = line
            .strip_prefix(&prefix)
            .ok_or("line-attribution marker header is not canonical")?;
        text_field(value)?;
        fields.insert(key, value);
    }
    if fields.get("format").copied() != Some("1") {
        return Err("line-attribution marker lacks format=1".into());
    }
    let output = fields
        .get("output")
        .copied()
        .ok_or("line-attribution marker lacks its output")?;
    let output_suffix = item
        .split_once('-')
        .and_then(|(_, name)| name.strip_prefix(output))
        .and_then(|suffix| suffix.strip_prefix('-'));
    if !output
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || output.starts_with('-')
        || output.ends_with('-')
        || output_suffix
            .and_then(|suffix| suffix.as_bytes().first())
            .is_none_or(|byte| !byte.is_ascii_digit())
    {
        return Err("line-attribution marker output does not name its runtime store item".into());
    }
    let runtime = fields
        .get("runtime")
        .copied()
        .ok_or("line-attribution marker lacks its runtime")?;
    let components: Result<Vec<_>, _> = Path::new(runtime)
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .ok_or("line-attribution marker runtime is not UTF-8"),
            _ => Err("line-attribution marker runtime is not canonical relative path"),
        })
        .collect();
    let components = components?;
    if components.is_empty() || components.join("/") != runtime {
        return Err("line-attribution marker runtime is not canonical relative path".into());
    }
    let ceiling = |key: &str| -> Result<u64, String> {
        let value = fields
            .get(key)
            .copied()
            .ok_or_else(|| format!("line-attribution marker lacks {key}"))?
            .parse::<u64>()
            .map_err(|_| format!("line-attribution marker {key} is not an integer"))?;
        if value == 0 {
            return Err(format!("line-attribution marker {key} is zero"));
        }
        Ok(value)
    };
    let reader = ceiling("reader_ceiling_bytes")?;
    let admitted = ceiling("admitted_ceiling_bytes")?;
    let companion = ceiling("companion_ceiling_bytes")?;
    if reader != crate::dwarf::MAX_LINE_SECTION_BYTES {
        return Err("line-attribution marker reader ceiling differs from td-profiler".into());
    }
    if reader >= admitted || admitted > companion {
        return Err("line-attribution marker ceilings are not ordered".into());
    }
    let reason = fields
        .get("reason")
        .copied()
        .ok_or("line-attribution marker lacks its reason")?;
    if reason.is_empty() {
        return Err("line-attribution marker has an empty reason".into());
    }
    Ok(PathBuf::from(runtime))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provenance {
    Source,
    Foreign,
}

struct ApplicationManifest {
    identity: String,
    runtime: String,
}

fn application_manifest(text: &str, expected_name: &str) -> Result<ApplicationManifest, String> {
    let fields = metadata_header(
        text,
        &[
            "name",
            "version",
            "alias?",
            "runtime",
            "entry",
            "provenance",
        ],
        "application manifest",
    )?;
    if fields.get("name").copied() != Some(expected_name) {
        return Err(format!(
            "application manifest identity disagrees with registry name {expected_name:?}"
        ));
    }
    provenance(
        fields
            .get("provenance")
            .copied()
            .ok_or("application manifest lacks provenance")?,
    )?;
    Ok(ApplicationManifest {
        identity: format!(
            "{}-{}",
            fields
                .get("name")
                .copied()
                .ok_or("application manifest lacks its name")?,
            fields
                .get("version")
                .copied()
                .ok_or("application manifest lacks its version")?
        ),
        runtime: fields
            .get("runtime")
            .copied()
            .ok_or("application manifest lacks its runtime")?
            .to_string(),
    })
}

fn application_runtime(text: &str, expected_name: &str) -> Result<PathBuf, String> {
    let fields = metadata_header(
        text,
        &["format", "name", "runtime", "entry"],
        "application spec",
    )?;
    if fields.get("format").copied() != Some("1")
        || fields.get("name").copied() != Some(expected_name)
    {
        return Err(format!(
            "application spec identity disagrees with registry name {expected_name:?}"
        ));
    }
    let runtime = PathBuf::from(
        fields
            .get("runtime")
            .copied()
            .ok_or("application spec lacks its runtime")?,
    );
    if !store_root(&runtime) {
        return Err("application spec runtime is not one canonical store root".into());
    }
    Ok(runtime)
}

fn metadata_header<'a>(
    text: &'a str,
    shape: &[&str],
    label: &str,
) -> Result<BTreeMap<&'a str, &'a str>, String> {
    if text.is_empty() || !text.ends_with('\n') || text.contains('\r') || text.contains('\0') {
        return Err(format!("{label} is not canonical text"));
    }
    let mut fields = BTreeMap::new();
    let mut keys = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('[') {
            break;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("{label} header is not key=value"))?;
        text_field(key)?;
        text_field(value)?;
        if fields.insert(key, value).is_some() {
            return Err(format!("{label} repeats key {key:?}"));
        }
        keys.push(key);
    }
    let required: Vec<_> = shape
        .iter()
        .filter(|key| !key.ends_with('?'))
        .copied()
        .collect();
    let with_optional: Vec<_> = shape.iter().map(|key| key.trim_end_matches('?')).collect();
    if keys != required && keys != with_optional {
        return Err(format!("{label} header is not canonical"));
    }
    Ok(fields)
}

fn read_metadata(path: &Path, limit: u64, label: &str) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat {label} {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > limit || metadata.mode() & 0o777 != 0o644 {
        return Err(format!(
            "{label} must be a bounded mode-0644 regular file: {}",
            path.display()
        ));
    }
    read_bounded_text(path, limit, label)
}

fn read_bounded_text(path: &Path, limit: u64, label: &str) -> Result<String, String> {
    let file =
        File::open(path).map_err(|error| format!("open {label} {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {label} {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{label} exceeds its {limit}-byte bound"));
    }
    String::from_utf8(bytes).map_err(|_| format!("{label} is not UTF-8: {}", path.display()))
}

fn image_path(root: &Path, target: &Path) -> Result<PathBuf, String> {
    let relative = target
        .strip_prefix("/")
        .map_err(|_| format!("target path is not absolute: {}", target.display()))?;
    Ok(root.join(relative))
}

fn store_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::RootDir))
        && matches!(components.next(), Some(Component::Normal(value)) if value == "td")
        && matches!(components.next(), Some(Component::Normal(value)) if value == "store")
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.all(|component| matches!(component, Component::Normal(_)))
}

fn store_root(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::RootDir))
        && matches!(components.next(), Some(Component::Normal(value)) if value == "td")
        && matches!(components.next(), Some(Component::Normal(value)) if value == "store")
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

fn field(path: &Path) -> Result<&str, String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("object index path is not UTF-8: {}", path.display()))?;
    text_field(value)
}

fn text_field(value: &str) -> Result<&str, String> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| matches!(byte, b'\t' | b'\n' | b'\r'))
    {
        return Err("object index field is empty or contains a delimiter".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{build, debug_path, registry_exclusions, validate_line_attribution_marker};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn elf_with_build_id() -> Vec<u8> {
        let mut elf = vec![0u8; 228];
        elf.get_mut(..7)
            .unwrap()
            .copy_from_slice(b"\x7fELF\x02\x01\x01");
        elf.get_mut(16..18)
            .unwrap()
            .copy_from_slice(&2u16.to_le_bytes());
        elf.get_mut(18..20)
            .unwrap()
            .copy_from_slice(&62u16.to_le_bytes());
        elf.get_mut(40..48)
            .unwrap()
            .copy_from_slice(&64u64.to_le_bytes());
        elf.get_mut(58..60)
            .unwrap()
            .copy_from_slice(&64u16.to_le_bytes());
        elf.get_mut(60..62)
            .unwrap()
            .copy_from_slice(&2u16.to_le_bytes());
        elf.get_mut(132..136)
            .unwrap()
            .copy_from_slice(&7u32.to_le_bytes());
        elf.get_mut(152..160)
            .unwrap()
            .copy_from_slice(&192u64.to_le_bytes());
        elf.get_mut(160..168)
            .unwrap()
            .copy_from_slice(&36u64.to_le_bytes());
        elf.get_mut(192..196)
            .unwrap()
            .copy_from_slice(&4u32.to_le_bytes());
        elf.get_mut(196..200)
            .unwrap()
            .copy_from_slice(&20u32.to_le_bytes());
        elf.get_mut(200..204)
            .unwrap()
            .copy_from_slice(&3u32.to_le_bytes());
        elf.get_mut(204..208).unwrap().copy_from_slice(b"GNU\0");
        elf.get_mut(208..228).unwrap().copy_from_slice(&[0x5a; 20]);
        elf
    }

    #[test]
    fn debug_paths_mirror_the_runtime_below_the_store_item() {
        assert_eq!(
            debug_path(Path::new("/td/store/hash-name/bin/program")).unwrap(),
            (
                PathBuf::from("/td/store/hash-name/lib/debug/bin/program.debug"),
                "hash-name".into(),
                PathBuf::from("bin/program")
            )
        );
    }

    #[test]
    fn image_index_is_sorted_and_excludes_foreign_roots() {
        let root =
            std::env::temp_dir().join(format!("td-profiler-index-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runtime = root.join("td/store/hash-native-1/bin/profiler");
        let debug = root.join("td/store/hash-native-1/lib/debug/bin/profiler.debug");
        let other = root.join("td/store/hash-native-1/bin/other");
        let other_debug = root.join("td/store/hash-native-1/lib/debug/bin/other.debug");
        let source = root.join("td/store/hash-source-1/bin/source");
        let foreign = root.join("td/store/hash-foreign-1/bin/payload");
        let foreign_runtime = root.join("td/store/hash-foreign-runtime-1/lib/runtime.so");
        for path in [&runtime, &debug, &source, &foreign, &foreign_runtime] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, elf_with_build_id()).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut other_elf = elf_with_build_id();
        *other_elf.get_mut(227).unwrap() = 0x5b;
        for path in [&other, &other_debug] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &other_elf).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let assembly = root.join("td/store/hash-native-1/lib/debug/.td-assembly-exception");
        std::fs::write(
            &assembly,
            b"format=1\noutput=native\nexception.0.source=runtime.S\nexception.0.reason=reviewed boundary\n",
        )
        .unwrap();
        std::fs::set_permissions(&assembly, std::fs::Permissions::from_mode(0o644)).unwrap();
        let line = root.join("td/store/hash-native-1/lib/debug/.td-line-attribution-exception");
        std::fs::write(
            &line,
            b"format=1\noutput=native\nruntime=bin/profiler\nreader_ceiling_bytes=33554432\nadmitted_ceiling_bytes=67108864\ncompanion_ceiling_bytes=134217728\nreason=bounded test fixture\n",
        )
        .unwrap();
        std::fs::set_permissions(&line, std::fs::Permissions::from_mode(0o644)).unwrap();
        for (package, manifest, spec) in [
            (
                "hash-foreign-1",
                "name=foreign\nversion=1\nruntime=native\nentry=/app/bin/payload\nprovenance=foreign\n",
                "format=1\nname=foreign\nruntime=/td/store/hash-native-1\nentry=/app/bin/payload\n",
            ),
            (
                "hash-source-1",
                "name=source\nversion=1\nruntime=foreign-runtime\nentry=/app/bin/source\nprovenance=foreign\n",
                "format=1\nname=source\nruntime=/td/store/hash-foreign-runtime-1\nentry=/app/bin/source\n",
            ),
        ] {
            let package = root.join("td/store").join(package);
            std::fs::write(package.join("manifest"), manifest).unwrap();
            std::fs::write(package.join("spec"), spec).unwrap();
            std::fs::set_permissions(
                package.join("manifest"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            std::fs::set_permissions(
                package.join("spec"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        let registry = root.join("etc/td-applications.tsv");
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(
            &registry,
            b"foreign\t/td/store/hash-foreign-1\nsource\t/td/store/hash-source-1\n",
        )
        .unwrap();
        let roots = root.join("etc/td-profiler-application-roots.tsv");
        std::fs::write(
            &roots,
            b"td-profiler-application-roots-v1\nforeign\tforeign-1\tforeign\tnative-1\tsource\nsource\tsource-1\tsource\tforeign-runtime-1\tforeign\n",
        )
        .unwrap();
        std::fs::set_permissions(&roots, std::fs::Permissions::from_mode(0o644)).unwrap();
        let output = root.join("etc/td-profiler-objects.tsv");
        let exclusions = registry_exclusions(&root, &registry, &roots).unwrap();
        build(&root, &output, &exclusions).unwrap();
        let contents = std::fs::read_to_string(output).unwrap();
        assert!(contents.starts_with("td-profiler-objects-v1\n"));
        assert!(contents.contains("/td/store/hash-native-1/bin/profiler\t"));
        assert!(contents.contains(
            "/td/store/hash-source-1/bin/source\t/td/store/hash-native-1/lib/debug/bin/profiler.debug"
        ));
        assert!(contents.contains("debug-store-item=hash-native-1"));
        assert!(contents
            .contains("store-item=hash-native-1;assembly-boundary=1;line-attribution-boundary=1"));
        let other_row = contents
            .lines()
            .find(|line| line.starts_with("/td/store/hash-native-1/bin/other\t"))
            .unwrap();
        assert!(other_row.ends_with(";assembly-boundary=1"));
        assert!(!other_row.contains("line-attribution-boundary=1"));
        assert!(!contents.contains("hash-foreign-1"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn line_attribution_marker_is_canonical_and_runtime_bound() {
        let marker = "format=1\noutput=codex\nruntime=bin/codex\nreader_ceiling_bytes=33554432\nadmitted_ceiling_bytes=167772160\ncompanion_ceiling_bytes=268435456\nreason=bounded reader exception\n";
        assert_eq!(
            validate_line_attribution_marker(marker, "hash-codex-0.148.0").unwrap(),
            PathBuf::from("bin/codex")
        );
        for invalid in [
            marker.replace("runtime=bin/codex", "runtime=../bin/codex"),
            marker.replace(
                "admitted_ceiling_bytes=167772160",
                "admitted_ceiling_bytes=16",
            ),
            marker.replace("reader_ceiling_bytes=33554432", "reader_ceiling_bytes=1"),
            marker.replace("output=codex", "output=other"),
            format!("{marker}trailing=value\n"),
            format!("{marker}\n[trailing]\nvalue=1\n"),
        ] {
            assert!(
                validate_line_attribution_marker(&invalid, "hash-codex-0.148.0").is_err(),
                "accepted non-canonical marker: {invalid:?}"
            );
        }
    }
}
