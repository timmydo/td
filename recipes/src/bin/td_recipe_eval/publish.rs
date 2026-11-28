//! Host-only release orchestration. The signer/uploader owns protocol validation.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use td_engine::{boot_info, boot_info_source, json, sha256};

const PROFILE: &str = "system-x86-64";
const HELP: &str = "usage: ./publish [--test] [--config FILE]\n\
Build and publish the current clean checkout. Tests are opt-in.\n\
Configuration: $XDG_CONFIG_HOME/td-publisher/publish.json (otherwise\n\
~/.config/td-publisher/publish.json). See net/OPERATIONS.md.\n\
Always verifies signatures and payloads. Keeps the newest `retain` ISOs\n\
of each profile (default 5) and the source mirror digests they pin,\n\
deleting older ones.\n\
Retry the same command after an interrupted upload or cleanup.";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    test: bool,
    config: Option<PathBuf>,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut result = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--test" if !result.test => result.test = true,
            "--config" if result.config.is_none() => {
                result.config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            _ => return Err(HELP.into()),
        }
    }
    Ok(result)
}

struct Config {
    root: PathBuf,
    key: PathBuf,
    repository: PathBuf,
    catalog: PathBuf,
    artifacts: PathBuf,
    credentials: PathBuf,
    endpoint: String,
    region: String,
    bucket: String,
    profile: String,
    retain: usize,
}

/// ISO releases of each profile kept when the configuration does not say.
const DEFAULT_RETAIN: usize = 5;
/// The ISO catalog's row limit (net/ISO.md); with several profiles the
/// catalog can still exceed it, which the index dry run refuses.
const MAX_RETAIN: usize = 128;

fn xdg(name: &str, home: &Path, fallback: &str) -> Result<PathBuf, String> {
    match std::env::var_os(name).filter(|v| !v.is_empty()) {
        Some(value) if Path::new(&value).is_absolute() => Ok(PathBuf::from(value)),
        Some(_) => Err(format!("{name} must be absolute")),
        None => Ok(home.join(fallback)),
    }
}

impl Config {
    fn parse(text: &str, base: &Path, home: &Path, data: &Path) -> Result<Self, String> {
        let json::Json::Obj(fields) = json::parse(text)? else {
            return Err("publisher configuration must be a JSON object".into());
        };
        let mut values = BTreeMap::new();
        let mut retain = None;
        for (key, value) in fields {
            if key == "retain" {
                let count = match &value {
                    json::Json::Num(text) => text.parse::<usize>().ok(),
                    _ => None,
                }
                .filter(|count| (1..=MAX_RETAIN).contains(count))
                .ok_or("publisher setting retain must be a whole number from 1 to 128")?;
                if retain.replace(count).is_some() {
                    return Err("duplicate publisher setting: retain".into());
                }
                continue;
            }
            if !matches!(
                key.as_str(),
                "root"
                    | "release-key"
                    | "repository"
                    | "catalog"
                    | "artifacts"
                    | "credentials"
                    | "endpoint"
                    | "region"
                    | "bucket"
                    | "profile"
            ) {
                return Err(format!("unknown publisher setting: {key}"));
            }
            let json::Json::Str(value) = value else {
                return Err(format!("publisher setting {key} must be a string"));
            };
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(format!(
                    "publisher setting {key} is empty or contains controls"
                ));
            }
            if values.insert(key.clone(), value).is_some() {
                return Err(format!("duplicate publisher setting: {key}"));
            }
        }
        let required = |key: &str| {
            values
                .get(key)
                .cloned()
                .ok_or_else(|| format!("missing publisher setting: {key}"))
        };
        let path = |key: &str, default: PathBuf| -> Result<PathBuf, String> {
            let Some(value) = values.get(key) else {
                return Ok(default);
            };
            let path = if let Some(tail) = value.strip_prefix("~/") {
                home.join(tail)
            } else {
                let path = Path::new(value);
                if path.is_absolute() {
                    path.to_owned()
                } else {
                    base.join(path)
                }
            };
            Ok(path)
        };
        Ok(Self {
            root: path("root", data.join("root"))?,
            key: path("release-key", data.join("signing/release-v1.key"))?,
            repository: path("repository", data.join("repository"))?,
            catalog: path("catalog", data.join("iso-catalog.tsv"))?,
            artifacts: path("artifacts", data.join("releases"))?,
            credentials: path("credentials", base.join("r2-credentials"))?,
            endpoint: required("endpoint")?,
            region: values
                .get("region")
                .cloned()
                .unwrap_or_else(|| "auto".into()),
            bucket: required("bucket")?,
            profile: values
                .get("profile")
                .cloned()
                .unwrap_or_else(|| "td-releases".into()),
            retain: retain.unwrap_or(DEFAULT_RETAIN),
        })
    }

    fn upload_args(&self) -> Vec<String> {
        vec![
            self.endpoint.clone(),
            self.region.clone(),
            self.bucket.clone(),
            path_text(&self.credentials),
            self.profile.clone(),
        ]
    }

    fn identity(&self) -> Result<String, String> {
        let mut parts = self.upload_args();
        parts.extend([
            path_text(&self.root),
            path_text(&self.key),
            path_text(&self.repository),
            path_text(&self.catalog),
            sha256::sha256_file(&self.root).map_err(|e| e.to_string())?,
        ]);
        Ok(parts.join("\n") + "\n")
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn read(path: &Path, limit: u64) -> Result<String, String> {
    let mut value = String::new();
    File::open(path)
        .and_then(|f| f.take(limit + 1).read_to_string(&mut value))
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if value.len() as u64 > limit {
        return Err(format!("{} is too large", path.display()));
    }
    Ok(value)
}

fn atomic(path: &Path, value: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("state file has no parent")?;
    let temporary = path.with_extension(format!("{}.partial", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|e| format!("create {}: {e}", temporary.display()))?;
    let result = (|| {
        file.write_all(value.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn directory(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| e.to_string())?;
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a private directory (0700)",
            path.display()
        ));
    }
    Ok(())
}

fn run(program: &Path, args: &[String]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("run {}: {e}", program.display()))?;
    if !status.success() {
        return Err(format!("{} failed: {status}", program.display()));
    }
    Ok(())
}

fn capture(program: &Path, args: &[String]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    let result = child
        .stdout
        .take()
        .ok_or("missing child stdout")?
        .take(65537)
        .read_to_string(&mut text);
    if result.is_err() || text.len() > 65536 {
        let _ = child.kill();
        let _ = child.wait();
        return Err("publisher child output is invalid or too large".into());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("{} failed: {status}", program.display()));
    }
    Ok(text.trim().to_owned())
}

/// Bring the local repository up to the bucket's release history, which is
/// authoritative: another host may have published since this one last did.
fn pull(net: &Path, config: &Config) -> Result<(), String> {
    let mut args = vec![path_text(&config.repository), path_text(&config.root)];
    args.extend(config.upload_args());
    run(net, &release_args("pull-s3", args))
}

fn release_args(verb: &str, args: Vec<String>) -> Vec<String> {
    let mut result = vec!["release".into(), verb.into()];
    result.extend(args);
    result
}

fn read_head(path: &Path) -> Result<String, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.to_string()),
        Ok(_) => read(path, 16384),
    }
}

// A lost promotion reply can leave our own index selected. Signature validation
// still belongs to upload-s3; this check only prevents an unintended new promotion.
fn check_selection(head: &str, baseline: &str, index: Option<&str>) -> Result<bool, String> {
    if head == baseline {
        return Ok(false);
    }
    if let Some(index) = index {
        if head.lines().next() == Some("td-release-head-v1")
            && head.lines().nth(5) == Some(PROFILE)
            && head.lines().nth(6) == Some(index)
        {
            return Ok(true);
        }
    }
    Err("selected head changed since this release began; refusing a superseded retry".into())
}

fn fingerprint(dir: &Path, iso: &Path) -> Result<String, String> {
    let mut text = String::new();
    for path in [
        dir.join("deployment/manifest"),
        dir.join("deployment/bzImage"),
        dir.join("deployment/initramfs.cpio"),
        dir.join("deployment/root.erofs"),
        iso.to_owned(),
    ] {
        text.push_str(&sha256::sha256_file(&path).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    Ok(text)
}

fn append_catalog(text: &str, row: &str, base: &Path) -> Result<String, String> {
    let mut lines = text.lines();
    if lines.next() != Some("td-iso-catalog-v1") {
        return Err("invalid ISO catalog header".into());
    }
    let mut output = String::from("td-iso-catalog-v1\n");
    let mut found = false;
    for line in lines {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 4 || line.chars().any(|c| c.is_control() && c != '\t') {
            return Err("invalid ISO catalog row".into());
        }
        let mut absolute = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            if field.is_empty() {
                return Err("empty ISO catalog field".into());
            }
            absolute.push(if index == 1 {
                field.to_string()
            } else {
                path_text(&base.join(field))
            });
        }
        let absolute = absolute.join("\t");
        found |= absolute == row;
        output.push_str(&absolute);
        output.push('\n');
    }
    if !found {
        output.push_str(row);
        output.push('\n');
    }
    Ok(output)
}

const CATALOG_HEADER: &str = "td-iso-catalog-v1";
const RETIRED_HEADER: &str = "td-iso-retired-v1";
const DIGESTS_HEADER: &str = "td-source-digests-v1";
const MAX_DIGEST_RECORD: u64 = 8 * 1024 * 1024;

fn basename(row: &str) -> Option<&str> {
    Path::new(row.rsplit('\t').next()?).file_name()?.to_str()
}

fn row_profile(row: &str) -> Option<&str> {
    row.split('\t').nth(1)
}

/// Keep the newest `keep` catalog rows of each profile, which
/// `append_catalog` holds in publication order, and retire the older ones.
/// A retired row whose ISO a kept row also lists (one commit signed under two
/// roots) is only unlisted: deleting it would take the kept row's image.
fn retain_rows(catalog: &str, keep: usize, current: &str) -> Result<(String, Vec<String>), String> {
    let mut lines = catalog.lines();
    if lines.next() != Some(CATALOG_HEADER) {
        return Err("invalid ISO catalog header".into());
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut marked = Vec::new();
    for row in lines.rev() {
        let count = seen
            .entry(row_profile(row).ok_or("invalid ISO catalog row")?)
            .or_insert(0);
        let kept = *count < keep;
        if kept {
            *count += 1;
        }
        marked.push((row, kept));
    }
    marked.reverse();
    if !marked.contains(&(current, true)) {
        return Err("the release being published is not among the newest kept".into());
    }
    let mut text = format!("{CATALOG_HEADER}\n");
    for (row, _) in marked.iter().filter(|(_, kept)| *kept) {
        text.push_str(row);
        text.push('\n');
    }
    let retired = still_retired(
        marked
            .iter()
            .filter(|(_, kept)| !*kept)
            .map(|(row, _)| row.to_string())
            .collect(),
        &text,
    );
    Ok((text, retired))
}

/// The retired rows `catalog` does not list again by ISO basename: a row
/// back in the listing (a raised `retain`, a hand edit) is no longer retired.
fn still_retired(rows: Vec<String>, catalog: &str) -> Vec<String> {
    let listed: Vec<_> = catalog.lines().skip(1).filter_map(basename).collect();
    rows.into_iter()
        .filter(|row| basename(row).is_some_and(|name| !listed.contains(&name)))
        .collect()
}

/// Retired ISO rows whose remote files are not yet deleted, beside the
/// catalog so a failed cleanup resumes on the next run.
fn retired_path(config: &Config) -> PathBuf {
    config.catalog.with_file_name("retired-isos.tsv")
}

fn read_retired(path: &Path) -> Result<Vec<String>, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(_) => {}
    }
    let text = read(path, 1024 * 1024)?;
    let mut lines = text.lines();
    if lines.next() != Some(RETIRED_HEADER) {
        return Err(format!("invalid retired ISO list {}", path.display()));
    }
    let rows: Vec<String> = lines.map(str::to_owned).collect();
    if rows.iter().any(|row| row.split('\t').count() != 4) {
        return Err(format!("invalid retired ISO row in {}", path.display()));
    }
    Ok(rows)
}

fn write_retired(path: &Path, rows: &[String]) -> Result<(), String> {
    if rows.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    atomic(
        path,
        &(format!("{RETIRED_HEADER}\n") + &rows.join("\n") + "\n"),
    )
}

/// A digest record: its header, its count, then one digest per line, so a
/// truncated or emptied copy is refused rather than read as fewer pins.
fn digests_text(digests: &std::collections::BTreeSet<String>) -> String {
    let mut text = format!("{DIGESTS_HEADER}\n{}\n", digests.len());
    for digest in digests {
        text.push_str(digest);
        text.push('\n');
    }
    text
}

fn parse_digests(text: &str) -> Result<Vec<&str>, String> {
    let mut lines = text.lines();
    if lines.next() != Some(DIGESTS_HEADER) {
        return Err("invalid source digest record header".into());
    }
    let count: usize = lines
        .next()
        .and_then(|line| line.parse().ok())
        .ok_or("invalid source digest record count")?;
    let digests: Vec<&str> = lines.collect();
    if count == 0
        || digests.len() != count
        || !digests
            .iter()
            .all(|d| d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    {
        return Err("source digest record is empty, truncated or malformed".into());
    }
    Ok(digests)
}

/// The digests this commit mirrored, which source mirror retention keeps
/// while its release is listed: the rosters `td-net s3 sources` uploads.
fn record_source_digests(dir: &Path) -> Result<(), String> {
    let mut digests = std::collections::BTreeSet::new();
    for pin in td_recipe::source_pins::all() {
        digests.insert(pin.sha256);
    }
    for pin in td_recipe::source_pins::build_root_pins()? {
        digests.insert(pin.sha256);
    }
    let text = digests_text(&digests);
    parse_digests(&text)?;
    atomic(&dir.join("source-digests"), &text)
}

enum MirrorKeep {
    /// Every listed release's digests.
    Keep(std::collections::BTreeSet<String>),
    /// A listed release has no record, so nothing may be collected yet.
    Unrecorded(PathBuf),
}

/// The union of the listed releases' digest records. `record` reads one,
/// `None` when it does not exist; any unreadable record is an error.
fn mirror_keep(
    catalog: &str,
    record: impl Fn(&Path) -> Result<Option<String>, String>,
) -> Result<MirrorKeep, String> {
    let mut keep = std::collections::BTreeSet::new();
    for row in catalog.lines().skip(1) {
        let path = Path::new(row.rsplit('\t').next().ok_or("invalid ISO catalog row")?)
            .parent()
            .ok_or("catalog ISO has no directory")?
            .join("source-digests");
        let Some(text) = record(&path)? else {
            return Ok(MirrorKeep::Unrecorded(path));
        };
        let digests = parse_digests(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        keep.extend(digests.into_iter().map(str::to_owned));
    }
    if keep.is_empty() {
        return Err("no listed release to keep the source mirror for".into());
    }
    Ok(MirrorKeep::Keep(keep))
}

fn read_record(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(_) => read(path, MAX_DIGEST_RECORD).map(Some),
    }
}

/// Delete what retention retired, after the trimmed listing is published:
/// each pending retired ISO and signature not listed again, then every
/// mirrored source no listed release pins. Failures stay pending.
fn retention(net: &Path, config: &Config, catalog: &str, dir: &Path) -> Result<(), String> {
    let mut failures = Vec::new();
    let pending = retired_path(config);
    match read_retired(&pending) {
        Err(e) => failures.push(e),
        Ok(rows) => {
            let mut remaining = Vec::new();
            for row in still_retired(rows, catalog) {
                let fields: Vec<&str> = row.split('\t').collect();
                let [root, profile, signature, _iso] = fields.as_slice() else {
                    failures.push(format!("invalid retired ISO row: {row}"));
                    remaining.push(row);
                    continue;
                };
                let mut args = vec![
                    "release".into(),
                    "retire-iso-s3".into(),
                    root.to_string(),
                    profile.to_string(),
                    signature.to_string(),
                ];
                args.extend(config.upload_args());
                args.extend(["--delete".into(), "--exclusive-publisher".into()]);
                if let Err(e) = run(net, &args) {
                    failures.push(format!("retire {signature}: {e}"));
                    remaining.push(row);
                }
            }
            if let Err(e) = write_retired(&pending, &remaining) {
                failures.push(e);
            }
        }
    }

    match mirror_keep(catalog, read_record) {
        Err(e) => failures.push(format!("source mirror retention: {e}")),
        Ok(MirrorKeep::Unrecorded(path)) => println!(
            "[publish] source mirror retention waits until every listed release has its digest record ({} is missing)",
            path.display()
        ),
        Ok(MirrorKeep::Keep(keep)) => {
            let list = dir.join("mirror-keep");
            let text: String = keep.iter().map(|digest| format!("{digest}\n")).collect();
            let mut args = vec!["s3".into(), "sources-gc".into(), path_text(&list)];
            args.extend(config.upload_args());
            args.extend(["--delete".into(), "--exclusive-publisher".into()]);
            if let Err(e) = atomic(&list, &text).and_then(|()| run(net, &args)) {
                failures.push(format!("source mirror cleanup: {e}"));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "published, but retention is incomplete; rerun ./publish, or see net/OPERATIONS.md, \"Storage retention\":\n{}",
            failures.join("\n")
        ))
    }
}

/// Internal child entry point: only the image build holds a compiler grant.
pub(crate) fn build_cli(args: &[String]) -> Result<(), String> {
    let [iso, deployment] = args else {
        return Err("usage: publish-build ISO DEPLOYMENT-DIRECTORY".into());
    };
    crate::check_runner::build_iso_cli(&["--out".into(), iso.clone()], Some(Path::new(deployment)))
}

fn build_image(builder: &Path, iso: &Path, deployment: &Path) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let executable =
        fs::canonicalize(executable).map_err(|e| format!("resolve build evaluator: {e}"))?;
    run(
        builder,
        &[
            "build-with-budget".into(),
            path_text(&executable),
            "publish-build".into(),
            path_text(iso),
            path_text(deployment),
        ],
    )
}

pub(crate) fn cli(args: &[String]) -> Result<(), String> {
    if args == ["--help"] || args == ["-h"] {
        println!("{HELP}");
        return Ok(());
    }
    let options = options(args)?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required")?);
    if !home.is_absolute() {
        return Err("HOME must be absolute".into());
    }
    let config_dir = xdg("XDG_CONFIG_HOME", &home, ".config")?.join("td-publisher");
    let data = xdg("XDG_DATA_HOME", &home, ".local/share")?.join("td-publisher");
    let config_path = options
        .config
        .unwrap_or_else(|| config_dir.join("publish.json"));
    let config_path = fs::canonicalize(&config_path).map_err(|e| {
        format!(
            "publisher config {}: {e}; see net/OPERATIONS.md",
            config_path.display()
        )
    })?;
    let config = Config::parse(
        &read(&config_path, 65536)?,
        config_path.parent().ok_or("config has no parent")?,
        &home,
        &data,
    )?;
    let repo = std::env::current_dir().map_err(|e| e.to_string())?;
    for path in [
        &repo,
        &home,
        &config.root,
        &config.key,
        &config.repository,
        &config.catalog,
        &config.artifacts,
        &config.credentials,
    ] {
        if path.to_str().is_none()
            || path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|b| b.is_ascii_control())
        {
            return Err("publisher paths must be UTF-8 without control characters".into());
        }
    }
    let source = boot_info_source::capture(&repo)?;
    if source.dirty {
        return Err("publish requires a clean checkout, including untracked files".into());
    }
    if source.commit.len() != 40 {
        return Err("release protocol requires a SHA-1 Git commit".into());
    }
    read(&config.catalog, 1024 * 1024)?;
    // A malformed pending list must stop the run here, before promotion.
    read_retired(&retired_path(&config))?;
    File::open(&config.credentials).map_err(|e| format!("open upload credentials: {e}"))?;
    File::open(&config.key).map_err(|e| format!("open release signing key: {e}"))?;
    directory(&config.artifacts)?;
    // A cache of the bucket's release history, created on first use.
    directory(&config.repository)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(config.repository.join("publish-workflow.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|e| format!("another publish workflow holds this repository: {e}"))?;
    let identity = config.identity()?;
    let root_hash = sha256::sha256_file(&config.root).map_err(|e| e.to_string())?;
    let dir = config
        .artifacts
        .join(format!("{}-{root_hash}", source.commit));
    directory(&dir)?;
    let identity_file = dir.join("configuration");
    if identity_file.exists() {
        if read(&identity_file, 65536)? != identity {
            return Err(
                "resume configuration differs; restore the original publisher configuration".into(),
            );
        }
    } else {
        atomic(&identity_file, &identity)?;
    }
    // Share the builder's isolated compiler/target selection with source warming.
    let builder = repo.join("target/release/td-builder");
    let net = crate::warm::resolve_td_net(&builder)?;
    let net = fs::canonicalize(net).map_err(|e| e.to_string())?;
    let target = fs::canonicalize(repo.join("net/target")).map_err(|e| e.to_string())?;
    if !net.starts_with(&target) {
        return Err("publishing requires td-net built from this checkout, not the deployed fallback; use a host with a C toolchain".into());
    }
    pull(&net, &config)?;
    let head = config.repository.join("heads").join(PROFILE);
    let baseline = dir.join("selected-head-before");
    let index_path = dir.join("index");
    let promoted = dir.join("promoted-head");
    // Removing the baseline before anything is staged is the operator's way
    // to publish this commit over a release selected since; the build stays.
    if !baseline.exists() {
        if index_path.exists() || promoted.exists() {
            return Err("retained publication lacks its original selected-head baseline".into());
        }
        atomic(&baseline, &read_head(&head)?)?;
    }
    let superseded = |error: String| {
        if index_path.exists() || promoted.exists() {
            error
        } else {
            format!(
                "{error}; another release was selected since this one began. To publish {} over it anyway, remove {} and rerun",
                source.commit,
                baseline.display()
            )
        }
    };
    let previous_head = read(&baseline, 16384)?;
    let saved_index = if index_path.exists() {
        Some(read(&index_path, 128)?)
    } else {
        None
    };
    check_selection(
        &read_head(&head)?,
        &previous_head,
        saved_index.as_deref().map(str::trim),
    )
    .map_err(superseded)?;
    if promoted.exists() && read(&promoted, 16384)? != read_head(&head)? {
        return Err(
            "another release is selected; refusing to republish a superseded release".into(),
        );
    }
    println!(
        "[publish] source {} ({}); tests {}",
        source.commit,
        source.commit_date,
        if options.test {
            "requested"
        } else {
            "not requested"
        }
    );
    let iso = dir.join(format!("td-install-x86-64-{}.iso", source.commit));
    let deployment = dir.join("deployment");
    let built = dir.join("built");
    if !built.exists() {
        if iso.exists() || deployment.exists() {
            return Err(format!(
                "incomplete build at {}; preserve or remove that ISO and deployment, then retry",
                dir.display()
            ));
        }
        crate::check_runner::warm_cli(&[PROFILE.into()])?;
        build_image(&builder, &iso, &deployment)?;
        boot_info_source::finish(&repo, &source)?;
        atomic(&built, &fingerprint(&dir, &iso)?)?;
    }
    if read(&built, 1024)? != fingerprint(&dir, &iso)? {
        return Err("retained release bytes changed; refusing to sign or upload".into());
    }
    let mut initramfs = File::open(deployment.join("initramfs.cpio")).map_err(|e| e.to_string())?;
    let info = boot_info::read(&mut initramfs)
        .map_err(|e| e.to_string())?
        .ok_or("deployment lacks build identity")?;
    if info.commit != source.commit || info.dirty {
        return Err("retained deployment has the wrong source identity".into());
    }
    if options.test {
        run(&builder, &["check".into()])?;
        crate::check_runner::qemu_admin_update_cli(&[])?;
        atomic(
            &dir.join("tests-passed"),
            "td-builder check\nqemu-admin-update\n",
        )?;
    }
    boot_info_source::finish(&repo, &source)?;
    // Mirror every pin of this commit before anything is signed or promoted,
    // so a published release never leaves a source build of it to upstream
    // alone (net/MIRROR.md). The image's warm covers only its closure; these
    // two cover every roster `s3 sources` reads, on every run.
    for roster in ["sources", "build-root"] {
        crate::warm::run_warm(&net, &repo, &builder, &["warm".into(), roster.into()]);
    }
    let mut sources = vec![
        "s3".into(),
        "sources".into(),
        path_text(&crate::check_runner::shared_sources_dir(Some(&home))),
    ];
    sources.extend(config.upload_args());
    run(&net, &sources)?;
    record_source_digests(&dir)?;
    let common = vec![
        path_text(&config.repository),
        path_text(&config.root),
        path_text(&config.key),
        PROFILE.into(),
    ];
    // The build took hours. A release selected meanwhile refuses below rather
    // than be superseded by this commit; one only staged is numbered above.
    pull(&net, &config)?;
    check_selection(
        &read_head(&head)?,
        &previous_head,
        saved_index.as_deref().map(str::trim),
    )
    .map_err(superseded)?;
    if !index_path.exists() {
        let mut args = common.clone();
        args.extend([source.commit.clone(), path_text(&deployment)]);
        let hash = capture(&net, &release_args("publish-next", args))?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("publisher returned an invalid index hash".into());
        }
        atomic(&index_path, &(hash + "\n"))?;
    }
    let signature_path = dir.join("signature-path");
    if !signature_path.exists() {
        let signature = capture(
            &net,
            &release_args(
                "sign-iso",
                vec![
                    path_text(&config.root),
                    path_text(&config.key),
                    PROFILE.into(),
                    source.commit.clone(),
                    path_text(&iso),
                    path_text(&dir.join("signatures")),
                ],
            ),
        )?;
        atomic(&signature_path, &(signature + "\n"))?;
    }
    let signature = read(&signature_path, 65536)?.trim().to_owned();
    run(
        &net,
        &release_args(
            "verify-iso",
            vec![
                path_text(&config.root),
                PROFILE.into(),
                signature.clone(),
                path_text(&iso),
            ],
        ),
    )?;
    let row = [
        path_text(&config.root),
        PROFILE.into(),
        signature,
        path_text(&iso),
    ]
    .join("\t");
    let catalog = append_catalog(
        &read(&config.catalog, 1024 * 1024)?,
        &row,
        config.catalog.parent().ok_or("catalog has no parent")?,
    )?;
    let (catalog, retired) = retain_rows(&catalog, config.retain, &row)?;
    let next_catalog = dir.join("iso-catalog.tsv");
    atomic(&next_catalog, &catalog)?;
    let mut iso_args = vec![path_text(&next_catalog), path_text(&dir.join("index.html"))];
    iso_args.extend(config.upload_args());
    let mut dry = iso_args.clone();
    dry.push("--dry-run".into());
    run(&net, &release_args("upload-iso-index-s3", dry))?;
    let index = read(&index_path, 128)?;
    let selected = read_head(&head)?;
    let already_selected = check_selection(&selected, &previous_head, Some(index.trim()))?;
    if !promoted.exists() {
        if !already_selected {
            let mut args = common;
            args.push(index.trim().into());
            run(&net, &release_args("promote-next", args))?;
        }
        atomic(&promoted, &read(&head, 16384)?)?;
    } else if read(&promoted, 16384)? != selected {
        return Err(
            "another release is selected; refusing to republish a superseded release".into(),
        );
    }
    let mut upload = vec![
        path_text(&config.repository),
        path_text(&config.root),
        PROFILE.into(),
    ];
    upload.extend(config.upload_args());
    run(&net, &release_args("upload-s3", upload))?;
    // Record what retires before the catalog forgets it, so a failed
    // cleanup resumes from the pending list on the next run.
    let mut pending = read_retired(&retired_path(&config))?;
    for row in retired {
        if !pending.contains(&row) {
            pending.push(row);
        }
    }
    write_retired(&retired_path(&config), &still_retired(pending, &catalog))?;
    // Keep the complete intended catalog even if the final page PUT loses its reply.
    atomic(&config.catalog, &catalog)?;
    run(&net, &release_args("upload-iso-index-s3", iso_args))?;
    atomic(
        &dir.join("published"),
        &format!("{}\n{}\n", source.commit, read(&index_path, 128)?.trim()),
    )?;
    println!(
        "[publish] published {}\n[publish] records and retry artifacts: {}",
        source.commit,
        dir.display()
    );
    // Only now that the listing no longer names them are old files deleted.
    retention(&net, &config, &catalog, &dir)?;
    println!(
        "[publish] retention: the newest {} ISO releases of each profile and their source digests are kept",
        config.retain
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn options_make_tests_explicit_and_reject_unknown_arguments() {
        assert!(!options(&[]).unwrap().test);
        assert!(options(&["--test".into()]).unwrap().test);
        for args in [
            vec!["--skip-verification".into()],
            vec!["--config".into()],
            vec!["--test".into(), "--test".into()],
        ] {
            assert!(options(&args).is_err());
        }
    }

    #[test]
    fn configuration_moves_between_users_and_resolves_relative_to_config() {
        let config = Config::parse(r#"{"endpoint":"https://s3.example","bucket":"td-releases","root":"../../root","release-key":"~/keys/release","credentials":"secret"}"#,
            Path::new("/home/timmy/.config/td-publisher"), Path::new("/home/timmy"), Path::new("/home/timmy/.local/share/td-publisher")).unwrap();
        assert_eq!(config.key, Path::new("/home/timmy/keys/release"));
        assert_eq!(
            config.credentials,
            Path::new("/home/timmy/.config/td-publisher/secret")
        );
        assert_eq!(
            config.repository,
            Path::new("/home/timmy/.local/share/td-publisher/repository")
        );
        for text in [
            r#"{"endpoint":"a","endpoint":"b","bucket":"c"}"#,
            r#"{"endpoint":"a","bucket":"c","typo":"value"}"#,
        ] {
            assert!(
                Config::parse(text, Path::new("/c"), Path::new("/h"), Path::new("/d")).is_err()
            );
        }
    }

    #[test]
    fn catalog_keeps_prior_entries_when_moved_and_retry_does_not_duplicate() {
        let old = "td-iso-catalog-v1\nroot\tsystem-x86-64\tsig\told.iso\n";
        let row = "/r\tsystem-x86-64\t/s\t/new.iso";
        let next = append_catalog(old, row, Path::new("/history")).unwrap();
        assert!(next.contains("/history/root\tsystem-x86-64\t/history/sig\t/history/old.iso\n"));
        assert_eq!(
            append_catalog(&next, row, Path::new("/elsewhere")).unwrap(),
            next
        );
        assert!(append_catalog("garbage", row, Path::new("/")).is_err());
    }

    #[test]
    fn retention_defaults_to_five_and_takes_a_bounded_count() {
        let parse = |extra: &str| {
            Config::parse(
                &format!(r#"{{"endpoint":"a","bucket":"c"{extra}}}"#),
                Path::new("/c"),
                Path::new("/h"),
                Path::new("/d"),
            )
        };
        assert_eq!(parse("").unwrap().retain, 5);
        assert_eq!(parse(r#","retain":2"#).unwrap().retain, 2);
        for bad in [
            r#","retain":0"#,
            r#","retain":129"#,
            r#","retain":"5""#,
            r#","retain":2.5"#,
            r#","retain":2,"retain":3"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn retention_keeps_the_newest_rows_and_spares_a_shared_image() {
        let row = |n: &str| format!("/r\tsystem-x86-64\t/s{n}\t/a{n}/td-{n}.iso");
        let shared = "/r2\tsystem-x86-64\t/s9\t/b/td-3.iso".to_string();
        let catalog = format!(
            "{CATALOG_HEADER}\n{}\n{}\n{shared}\n{}\n{}\n",
            row("1"),
            row("2"),
            row("3"),
            row("4")
        );

        let (kept, retired) = retain_rows(&catalog, 2, &row("4")).unwrap();
        assert_eq!(
            kept,
            format!("{CATALOG_HEADER}\n{}\n{}\n", row("3"), row("4"))
        );
        // td-3.iso stays listed through row 3, so its other row only unlists.
        assert_eq!(retired, [row("1"), row("2")]);

        let (kept, retired) = retain_rows(&catalog, 5, &row("4")).unwrap();
        assert_eq!(kept, catalog);
        assert!(retired.is_empty());
        assert!(retain_rows(&catalog, 1, &row("3")).is_err());
        assert!(retain_rows("garbage", 1, &row("1")).is_err());
    }

    #[test]
    fn retention_counts_each_profile_on_its_own() {
        let desktop = |n: &str| format!("/r\tsystem-x86-64\t/s{n}\t/a{n}/td-{n}.iso");
        let server = "/r\tserver-x86-64\t/srv\t/srv/td-server.iso".to_string();
        let catalog = format!(
            "{CATALOG_HEADER}\n{server}\n{}\n{}\n{}\n",
            desktop("1"),
            desktop("2"),
            desktop("3")
        );
        let (kept, retired) = retain_rows(&catalog, 2, &desktop("3")).unwrap();
        assert_eq!(
            kept,
            format!(
                "{CATALOG_HEADER}\n{server}\n{}\n{}\n",
                desktop("2"),
                desktop("3")
            )
        );
        assert_eq!(retired, [desktop("1")]);
    }

    #[test]
    fn a_pending_row_listed_again_is_no_longer_retired() {
        let listed = "/r\tsystem-x86-64\t/s1\t/a1/td-1.iso".to_string();
        let gone = "/r\tsystem-x86-64\t/s0\t/a0/td-0.iso".to_string();
        let catalog = format!("{CATALOG_HEADER}\n{listed}\n");
        // The same ISO under another signature is listed too, by basename.
        let resigned = "/r2\tsystem-x86-64\t/s9\t/b/td-1.iso".to_string();
        assert_eq!(
            still_retired(vec![listed, gone.clone(), resigned], &catalog),
            [gone]
        );
    }

    #[test]
    fn digest_records_refuse_truncation_and_union_over_listed_releases() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let set = |items: &[&String]| {
            items
                .iter()
                .map(|s| s.to_string())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let one = digests_text(&set(&[&a, &b]));
        assert_eq!(parse_digests(&one).unwrap(), [a.as_str(), b.as_str()]);
        let truncated = one.rsplit_once(&b).unwrap().0;
        for bad in [
            "",
            truncated,
            "td-source-digests-v1\n0\n",
            "td-source-digests-v1\n1\nNOT-A-DIGEST\n",
            "a-different-header\n1\n",
        ] {
            assert!(parse_digests(bad).is_err(), "{bad:?}");
        }

        let catalog = format!(
            "{CATALOG_HEADER}\n/r\tsystem-x86-64\t/s1\t/a1/td-1.iso\n/r\tsystem-x86-64\t/s2\t/a2/td-2.iso\n"
        );
        let records = |path: &Path| {
            Ok(match path.to_str().unwrap() {
                "/a1/source-digests" => Some(digests_text(&set(&[&a]))),
                "/a2/source-digests" => Some(digests_text(&set(&[&b]))),
                _ => None,
            })
        };
        assert!(matches!(
            mirror_keep(&catalog, records).unwrap(),
            MirrorKeep::Keep(keep) if keep == set(&[&a, &b])
        ));

        let unrecorded = |path: &Path| {
            Ok((path == Path::new("/a1/source-digests")).then(|| digests_text(&set(&[&a]))))
        };
        assert!(matches!(
            mirror_keep(&catalog, unrecorded).unwrap(),
            MirrorKeep::Unrecorded(path) if path == Path::new("/a2/source-digests")
        ));
        let broken = |_: &Path| Ok(Some(truncated.to_string()));
        assert!(mirror_keep(&catalog, broken).is_err());
        let failing = |_: &Path| Err("unreadable".to_string());
        assert!(mirror_keep(&catalog, failing).is_err());
        assert!(mirror_keep(CATALOG_HEADER, records).is_err());
    }

    #[test]
    fn the_retired_list_round_trips_and_empties_away() {
        let dir = crate::test_temp::TempDir::new("td-publish-retired").unwrap();
        let path = dir.path().join("retired-isos.tsv");
        assert!(read_retired(&path).unwrap().is_empty());
        let rows = ["/r\tsystem-x86-64\t/s\t/a.iso".to_string()];
        write_retired(&path, &rows).unwrap();
        assert_eq!(read_retired(&path).unwrap(), rows);
        write_retired(&path, &[]).unwrap();
        assert!(!path.exists());
        write_retired(&path, &[]).unwrap();
        fs::write(&path, "td-iso-retired-v1\nnot four fields\n").unwrap();
        assert!(read_retired(&path).is_err());
    }

    #[test]
    fn interrupted_signing_cannot_supersede_an_intervening_release() {
        let before = "td-release-head-v1\n2\n1\n0\n0\nsystem-x86-64\nold\nsig\n";
        let other = "td-release-head-v1\n2\n2\n0\n0\nsystem-x86-64\nother\nsig\n";
        let ours = "td-release-head-v1\n2\n3\n0\n0\nsystem-x86-64\nours\nsig\n";
        // A crash after signing but before saving its index must not allocate a
        // fresh sequence over a release selected by another checkout meanwhile.
        assert!(check_selection(other, before, None).is_err());
        assert!(check_selection(other, before, Some("ours")).is_err());
        assert!(!check_selection(before, before, None).unwrap());
        // A crash after our own promotion can finish upload without re-promotion.
        assert!(check_selection(ours, before, Some("ours")).unwrap());
    }

    #[test]
    fn state_is_durable_and_payload_changes_break_the_resume_fingerprint() {
        let dir = crate::test_temp::TempDir::new("td-publish-state").unwrap();
        let dir = dir.path();
        fs::create_dir(dir.join("deployment")).unwrap();
        for name in ["manifest", "bzImage", "initramfs.cpio", "root.erofs"] {
            fs::write(dir.join("deployment").join(name), name).unwrap();
        }
        let iso = dir.join("image.iso");
        fs::write(&iso, b"iso").unwrap();
        let before = fingerprint(dir, &iso).unwrap();
        atomic(&dir.join("built"), &before).unwrap();
        fs::write(dir.join("deployment/root.erofs"), b"changed").unwrap();
        assert_ne!(
            read(&dir.join("built"), 1024).unwrap(),
            fingerprint(dir, &iso).unwrap()
        );
        atomic(&dir.join("built"), "replacement").unwrap();
        assert_eq!(read(&dir.join("built"), 1024).unwrap(), "replacement");
    }
}
