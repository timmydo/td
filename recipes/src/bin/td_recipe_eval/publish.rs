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
of each profile (default 5), listed in the bucket's ISO catalog, and the\n\
source mirror digests they pin, deleting older ones. Any host may run it:\n\
release history and the ISO catalog live in the bucket.\n\
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
/// The download page's entry limit (net/ISO.md); with several profiles the
/// listing can still exceed it, which `list-iso-s3 --check` refuses.
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

const DIGESTS_HEADER: &str = "td-source-digests-v1";

/// A digest record: its header, its count, then one digest per line, so a
/// truncated or emptied copy is refused rather than read as fewer pins
/// (td-net's `list-iso-s3` reads it).
fn digests_text(digests: &std::collections::BTreeSet<String>) -> String {
    let mut text = format!("{DIGESTS_HEADER}\n{}\n", digests.len());
    for digest in digests {
        text.push_str(digest);
        text.push('\n');
    }
    text
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
    if digests.is_empty() {
        return Err("no source pins to record".into());
    }
    atomic(&dir.join("source-digests"), &digests_text(&digests))
}

/// Seed a bucket that holds no ISO catalog from this host's pre-bucket one,
/// which td-net then sets aside. Before the build, it refuses a bucket with
/// no catalog and a host with none, or a bucket's catalog that lacks
/// releases this host's local one lists.
fn import_catalog(net: &Path, config: &Config, dir: &Path) -> Result<(), String> {
    let mut args = vec![
        path_text(&config.root),
        path_text(&config.catalog),
        path_text(&config.catalog.with_file_name("retired-isos.tsv")),
        path_text(dir),
    ];
    args.extend(config.upload_args());
    run(net, &release_args("import-iso-catalog-s3", args))
}

/// List this release in the bucket's ISO catalog and republish the page;
/// `check` only confirms, before promotion, that the listing would succeed.
fn list_iso(
    net: &Path,
    config: &Config,
    dir: &Path,
    signature: &str,
    iso: &Path,
    check: bool,
) -> Result<(), String> {
    let mut args = vec![
        path_text(&config.root),
        PROFILE.into(),
        signature.into(),
        path_text(iso),
        path_text(&dir.join("source-digests")),
        config.retain.to_string(),
        path_text(dir),
    ];
    args.extend(config.upload_args());
    if check {
        args.push("--check".into());
    }
    run(net, &release_args("list-iso-s3", args))
}

/// Delete what the catalog retired, then mirrored sources no listed release
/// pins; td-net keeps failures pending in the bucket's catalog.
fn retire(net: &Path, config: &Config, dir: &Path) -> Result<(), String> {
    let mut args = vec![path_text(&config.root), path_text(dir)];
    args.extend(config.upload_args());
    args.extend(["--delete".into(), "--exclusive-publisher".into()]);
    run(net, &release_args("retire-s3", args)).map_err(|e| {
        format!(
            "published, but retention is incomplete ({e}); rerun ./publish, or see net/OPERATIONS.md, \"Storage retention\""
        )
    })
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
    // Seeds the bucket once from a host that published before the catalog
    // moved, and refuses here, not after the build, when neither has one.
    import_catalog(&net, &config, &dir)?;
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
    // Before promotion: the bucket's catalog must take this release.
    list_iso(&net, &config, &dir, &signature, &iso, true)?;
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
    // The bucket's catalog records the listing and what it retires before
    // the page; a rerun rebuilds a lost page from it.
    list_iso(&net, &config, &dir, &signature, &iso, false)?;
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
    retire(&net, &config, &dir)?;
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
    fn digest_records_carry_a_header_and_count() {
        let digests: std::collections::BTreeSet<String> =
            ["b".repeat(64), "a".repeat(64)].into_iter().collect();
        assert_eq!(
            digests_text(&digests),
            format!(
                "{DIGESTS_HEADER}\n2\n{}\n{}\n",
                "a".repeat(64),
                "b".repeat(64)
            )
        );
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
