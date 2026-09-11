//! Resolve recipe-owned vendor jobs and prepare private caches without egress.
use super::*;

mod metadata;

const MAX_JOBS: usize = 128;
const MAX_PLAN_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum Source {
    Local,
    Crate { name: String, version: String },
    Archive { file: String, checksum: String },
}

#[derive(Debug, PartialEq, Eq)]
struct Job {
    dest: String,
    lock: String,
    source: Source,
}

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

fn parse_jobs(text: &str) -> Result<Vec<Job>, String> {
    if text.len() as u64 > MAX_PLAN_BYTES {
        return Err("vendor plan exceeds byte limit".into());
    }
    let mut jobs = Vec::new();
    let mut destinations = std::collections::BTreeSet::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split('\t').collect();
        let job = match fields.as_slice() {
            ["warm", "crate-local", source, dest, lock]
                if relative(source) && Path::new(lock) == Path::new(source).join("Cargo.lock") =>
            {
                Job {
                    dest: (*dest).into(),
                    lock: (*lock).into(),
                    source: Source::Local,
                }
            }
            ["warm", "crate", name, version, dest, lock]
                if index_path(name).is_some()
                    && !version.is_empty()
                    && version.len() <= 128
                    && version.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_')
                    })
                    && *lock
                        == format!(
                            ".td-build-cache/crate-vendor/{dest}/src/{name}-{version}/Cargo.lock"
                        ) =>
            {
                Job {
                    dest: (*dest).into(),
                    lock: (*lock).into(),
                    source: Source::Crate {
                        name: (*name).into(),
                        version: (*version).into(),
                    },
                }
            }
            ["warm", "crate-source", file, checksum, lock, dest, repeated]
                if lock == repeated
                    && valid_crate_source_coordinates(file, checksum, lock, dest) =>
            {
                Job {
                    dest: (*dest).into(),
                    lock: (*lock).into(),
                    source: Source::Archive {
                        file: (*file).into(),
                        checksum: (*checksum).into(),
                    },
                }
            }
            _ => return Err("unrecognized or unsafe recipe vendor plan row".into()),
        };
        if !plain_name(&job.dest) || !relative(&job.lock) || !destinations.insert(job.dest.clone())
        {
            return Err(format!(
                "unsafe or repeated vendor destination {}",
                job.dest
            ));
        }
        if jobs.len() >= MAX_JOBS {
            return Err("vendor plan exceeds job limit".into());
        }
        jobs.push(job);
    }
    Ok(jobs)
}

pub(super) fn prepare_package_metadata(
    archive: &Path,
    checksum: &str,
    package: &str,
    source: &Path,
) -> Result<PathBuf, String> {
    let selected = metadata::read_pinned(archive, checksum, package)?;
    std::fs::create_dir_all(source).map_err(|e| e.to_string())?;
    let lock = source.join("Cargo.lock");
    let manifest = source.join("Cargo.toml");
    write_atomic(&lock, selected.lock.as_bytes())?;
    write_atomic(&manifest, selected.manifest.as_bytes())?;
    detach_from_workspace(&manifest)?;
    Ok(lock)
}

pub(super) fn command_text(command: Command, label: &str, limit: u64) -> Result<String, String> {
    command_text_before(
        command,
        label,
        limit,
        Instant::now() + SOURCE_CONSUMER_TIMEOUT,
    )
}

fn command_text_before(
    mut command: Command,
    label: &str,
    limit: u64,
    deadline: Instant,
) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start {label}: {e}"))?;
    let Some(output) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{label} has no stdout"));
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let result = output
            .take(limit + 1)
            .read_to_string(&mut text)
            .map(|_| text)
            .map_err(|e| e.to_string());
        let _ = sender.send(result);
    });
    let result = (|| {
        let text = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| format!("read {label} within deadline: {e}"))??;
        if text.len() as u64 > limit {
            return Err(format!("{label} exceeds {limit}-byte limit"));
        }
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("wait for {label}: {e}"))?
            {
                if !status.success() {
                    return Err(format!("{label} failed: {status}"));
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("{label} exceeded deadline"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(text)
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        // A descendant may still hold stdout. Detach this bounded reader on
        // failure instead of turning its join into an unbounded caller wait.
        return result;
    }
    reader
        .join()
        .map_err(|_| format!("{label} reader failed"))?;
    result
}

fn recipe_jobs(root: &Path, target: Option<&str>) -> Result<Vec<Job>, String> {
    let mut command = Command::new(recipe_eval_tool(root)?);
    command.arg("vendor-warm-args").current_dir(root);
    if let Some(target) = target {
        command.arg(target);
    }
    parse_jobs(&command_text(command, "vendor planner", MAX_PLAN_BYTES)?)
}

fn source_pin<'a>(job: &Job, pins: &'a [SourcePin]) -> Result<Option<&'a SourcePin>, String> {
    let (file, checksum) = match &job.source {
        Source::Local => return Ok(None),
        Source::Crate { name, version } => (format!("{name}-{version}.crate"), None),
        Source::Archive { file, checksum } => (file.clone(), Some(checksum.as_str())),
    };
    let mut matches = pins.iter().filter(|pin| pin.file == file);
    let pin = matches
        .next()
        .ok_or_else(|| format!("{} has no declared source pin for {file}", job.dest))?;
    if matches.any(|other| other.sha256 != pin.sha256)
        || checksum.is_some_and(|want| want != pin.sha256)
    {
        return Err(format!(
            "{} source pin is ambiguous or mismatched for {file}",
            job.dest
        ));
    }
    source_feed_path(pin)?;
    Ok(Some(pin))
}

fn existing_dir(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(format!(
            "vendor state is not a real directory: {}",
            path.display()
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("inspect {}: {e}", path.display())),
    }
}

fn recover(current: &Path, stage: &Path, previous: &Path) -> Result<(), String> {
    let have_current = existing_dir(current)?;
    if existing_dir(previous)? {
        if have_current {
            std::fs::remove_dir_all(previous)
        } else {
            std::fs::rename(previous, current)
        }
        .map_err(|e| format!("recover vendor cache: {e}"))?;
    }
    if existing_dir(stage)? {
        std::fs::remove_dir_all(stage)
            .map_err(|e| format!("remove interrupted vendor staging: {e}"))?;
    }
    Ok(())
}

fn publish(current: &Path, stage: &Path, previous: &Path) -> Result<(), String> {
    let replaced = existing_dir(current)?;
    if replaced {
        std::fs::rename(current, previous)
            .map_err(|e| format!("retain previous vendor cache: {e}"))?;
    }
    if let Err(error) = std::fs::rename(stage, current) {
        if replaced {
            std::fs::rename(previous, current).map_err(|e| {
                format!("publish vendor cache failed: {error}; rollback failed: {e}")
            })?;
        }
        return Err(format!("publish vendor cache: {error}"));
    }
    if replaced {
        if let Err(error) = std::fs::remove_dir_all(previous) {
            eprintln!(">> td-feed: vendor cache published; previous cache cleanup failed: {error}; retry will clean it up");
        }
    }
    Ok(())
}

fn reuse_archive(source: &Path, destination: &Path, checksum: &str) {
    if matches!(std::fs::symlink_metadata(source), Err(e) if e.kind() == io::ErrorKind::NotFound) {
        return;
    }
    if let Err(error) = copy_verified_file(source, destination, checksum, || Ok(())) {
        eprintln!(
            ">> td-feed: local archive {} could not be reused ({error}); checking the host feed",
            source.display()
        );
    }
}

pub(super) fn lock_job(root: &Path, dest: &str) -> Result<File, String> {
    lock_job_before(root, dest, Instant::now() + SOURCE_CONSUMER_TIMEOUT)
}

fn lock_job_before(root: &Path, dest: &str, deadline: Instant) -> Result<File, String> {
    if !plain_name(dest) {
        return Err("vendor destination must be a plain recipe name".into());
    }
    let parent = root.join(".td-build-cache/crate-vendor");
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(OPEN_REGULAR_NOFOLLOW)
        .open(parent.join(format!(".{}.feed-lock", dest)))
        .map_err(|e| format!("open vendor preparation lock: {e}"))?;
    if !lock.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("vendor preparation lock is not a regular file".into());
    }
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(format!("{} vendor preparation is busy", dest));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(format!("lock vendor preparation: {e}"))
            }
        }
    }
    Ok(lock)
}

fn consume_job(
    root: &Path,
    job: &Job,
    pin: Option<&SourcePin>,
    base: &str,
    source_cache: &Path,
) -> Result<(), String> {
    let _lock = lock_job(root, &job.dest)?;
    let parent = root.join(".td-build-cache/crate-vendor");
    let current = parent.join(&job.dest);
    let stage = parent.join(format!(".{}.feed-new", job.dest));
    let previous = parent.join(format!(".{}.feed-old", job.dest));
    recover(&current, &stage, &previous)?;
    std::fs::create_dir(&stage).map_err(|e| format!("create vendor staging: {e}"))?;
    let result = (|| {
        let lock_path = match &job.source {
            Source::Crate { name, version } => {
                let pin = pin.ok_or("crate job has no source pin")?;
                let work = stage.join("work");
                reuse_archive(
                    &current.join("work").join(&pin.file),
                    &work.join(&pin.file),
                    &pin.sha256,
                );
                if !work.join(&pin.file).is_file() {
                    reuse_archive(
                        &source_cache.join(&pin.file),
                        &work.join(&pin.file),
                        &pin.sha256,
                    );
                }
                consume_source_pins(std::slice::from_ref(pin), &work, base)?;
                let package = format!("{name}-{version}");
                let src = stage.join("src").join(&package);
                prepare_package_metadata(&work.join(&pin.file), &pin.sha256, &package, &src)?
            }
            Source::Archive { .. } => {
                consume_source_pins(
                    std::slice::from_ref(pin.ok_or("archive job has no source pin")?),
                    source_cache,
                    base,
                )?;
                real_relative_file(root, &job.lock, "committed Cargo.lock")?
            }
            Source::Local => real_relative_file(root, &job.lock, "local Cargo.lock")?,
        };
        let (sources, digest) = read_locked_cargo_sources(&lock_path)?;
        let vendor = stage.join("vendor");
        std::fs::create_dir(&vendor).map_err(|e| e.to_string())?;
        for package in &sources.registry {
            let name = registry_archive_name(package);
            reuse_archive(
                &current.join("vendor").join(&name),
                &vendor.join(&name),
                &package.checksum,
            );
        }
        transfer_registry_sources(&sources, &vendor, RegistryTransfer::Consume(base))?;
        if count_crates(&vendor) != sources.registry.len() {
            return Err("staged vendor archive set does not match the selected lock".into());
        }
        if lock_digest(&lock_path).as_deref() != Some(&digest) {
            return Err("selected Cargo.lock changed during vendor preparation; retry".into());
        }
        write_atomic(
            &vendor.join(".warm-complete"),
            warm_complete_text(sources.registry.len(), &digest).as_bytes(),
        )?;
        publish(&current, &stage, &previous)
    })();
    finish_staging(result, &stage)
}

fn finish_staging(result: Result<(), String>, stage: &Path) -> Result<(), String> {
    result.map_err(|error| {
        let cleanup = existing_dir(stage).and_then(|exists| {
            if exists {
                std::fs::remove_dir_all(stage).map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        });
        match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; clean failed vendor staging: {cleanup}"),
        }
    })
}

pub(super) fn run(root: &Path, target: Option<&str>, consume: bool) -> Result<(), String> {
    let base = if consume {
        Some(configured_consumer_feed_base()?)
    } else {
        None
    };
    let jobs = recipe_jobs(root, target)?;
    let pins = if jobs.iter().any(|job| job.source != Source::Local) {
        recipe_source_pins_result(root)?
    } else {
        Vec::new()
    };
    let mut failures = Vec::new();
    for job in &jobs {
        let result = (|| {
            let pin = source_pin(job, &pins)?;
            if let Some(base) = &base {
                consume_job(root, job, pin, base, &sources_dir())
            } else {
                let _lock = lock_job(root, &job.dest)?;
                if let Some(pin) = pin {
                    let cache = sources_dir();
                    let source_cache = if matches!(job.source, Source::Crate { .. })
                        && !cache.join(&pin.file).is_file()
                    {
                        root.join(".td-build-cache/crate-vendor")
                            .join(&job.dest)
                            .join("work")
                    } else {
                        cache
                    };
                    export_source_pin(pin, &source_cache, &feed_dir().join("store"))?;
                }
                let lock = real_relative_file(root, &job.lock, "selected Cargo.lock")?;
                let archives = root
                    .join(".td-build-cache/crate-vendor")
                    .join(&job.dest)
                    .join("vendor");
                transfer_locked_registry(&lock, &archives, &feed_dir().join("store"), None)
            }
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error}", job.dest));
        }
    }
    if failures.is_empty() {
        eprintln!(
            ">> td-feed: {} recipe vendor set(s) {}",
            jobs.len(),
            if consume { "prepared" } else { "exported" }
        );
        Ok(())
    } else {
        Err(format!(
            "{} vendor job(s) failed:\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::tests::ConsumerServer;

    pub(super) fn scratch(tag: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-vendor-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn lock_text(bytes: &[u8]) -> String {
        format!("version = 4\n[[package]]\nname = \"dep\"\nversion = \"1.0.0\"\nsource = \"registry+https://example.invalid/index\"\nchecksum = \"{}\"\n", hex_sha256(bytes))
    }

    fn local(root: &Path, bytes: &[u8]) -> Job {
        std::fs::create_dir_all(root.join("net")).unwrap();
        std::fs::write(root.join("net/Cargo.lock"), lock_text(bytes)).unwrap();
        Job {
            dest: "td-net".into(),
            lock: "net/Cargo.lock".into(),
            source: Source::Local,
        }
    }

    fn host_archive(root: &Path, store: &Path, bytes: &[u8]) {
        let lock = root.join("Cargo.lock");
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(&lock, lock_text(bytes)).unwrap();
        std::fs::write(root.join("dep-1.0.0.crate"), bytes).unwrap();
        transfer_locked_registry(&lock, root, store, None).unwrap();
    }

    fn vendor(root: &Path) -> PathBuf {
        root.join(".td-build-cache/crate-vendor/td-net/vendor")
    }

    #[test]
    fn recipe_rows_are_typed_and_cannot_choose_commands_or_escape_destinations() {
        let valid = "warm\tcrate-local\tnet\ttd-net\tnet/Cargo.lock\n\
                     warm\tcrate\tcoreutils\t0.9.0\tuutils\t.td-build-cache/crate-vendor/uutils/src/coreutils-0.9.0/Cargo.lock\n\
                     warm\tcrate-source\tsource.tar.gz\tCHECKSUM\trecipes/locks/codex/Cargo.lock\tcodex\trecipes/locks/codex/Cargo.lock\n".replace("CHECKSUM", &"a".repeat(64));
        assert_eq!(parse_jobs(&valid).unwrap().len(), 3);
        for text in [
            valid.replace("crate-local", "shell"),
            valid.replace("td-net", "../td-net"),
            valid.replace("net/Cargo.lock", "/tmp/Cargo.lock"),
            valid.replace("0.9.0", "../0.9.0"),
            valid.replace("source.tar.gz", "../source.tar.gz"),
            format!("{valid}{valid}"),
        ] {
            assert!(parse_jobs(&text).is_err(), "{text}");
        }
        assert!(parse_jobs("").unwrap().is_empty());
    }

    #[test]
    fn subprocess_pipe_fixture() {
        let Some(mode) = std::env::var_os("TD_VENDOR_PIPE_FIXTURE") else {
            return;
        };
        let dir = PathBuf::from(std::env::var_os("TD_VENDOR_PIPE_DIR").unwrap());
        if mode == "parent" {
            let mut descendant = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "feed::vendor::tests::subprocess_pipe_fixture",
                    "--nocapture",
                ])
                .env("TD_VENDOR_PIPE_FIXTURE", "descendant")
                .spawn()
                .unwrap();
            std::fs::write(dir.join("started"), b"ready").unwrap();
            let _ = descendant.wait();
        } else {
            std::thread::sleep(Duration::from_secs(6));
            std::fs::write(dir.join("finished"), b"done").unwrap();
        }
    }

    #[test]
    fn a_descendants_inherited_stdout_cannot_extend_the_command_deadline() {
        let dir = scratch("pipe-deadline");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "feed::vendor::tests::subprocess_pipe_fixture",
                "--nocapture",
            ])
            .env("TD_VENDOR_PIPE_FIXTURE", "parent")
            .env("TD_VENDOR_PIPE_DIR", &dir);
        let start = Instant::now();
        let result = command_text_before(
            command,
            "pipe fixture",
            4096,
            start + Duration::from_secs(2),
        );
        let elapsed = start.elapsed();
        assert!(
            dir.join("started").is_file(),
            "fixture did not start within its deadline"
        );
        // Let our bounded descendant finish before removing its scratch state.
        let cleanup_deadline = Instant::now() + Duration::from_secs(10);
        while !dir.join("finished").is_file() && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dir.join("finished").is_file());
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(4),
            "deadline extended to {elapsed:?}"
        );
    }

    #[test]
    fn identical_source_pin_aliases_do_not_make_a_vendor_plan_ambiguous() {
        let job = Job {
            dest: "fixture".into(),
            lock: "unused".into(),
            source: Source::Crate {
                name: "fixture".into(),
                version: "1.0.0".into(),
            },
        };
        let pin = SourcePin {
            key: "first".into(),
            file: "fixture-1.0.0.crate".into(),
            url: "https://example.invalid/fixture".into(),
            sha256: "a".repeat(64),
        };
        let mut alias = pin.clone();
        alias.key = "alias".into();
        assert!(source_pin(&job, &[pin.clone(), alias.clone()]).is_ok());
        alias.sha256 = "b".repeat(64);
        assert!(source_pin(&job, &[pin, alias]).is_err());
        assert!(parse_jobs("warm\tcrate-local\tnet/\ttd-net\tnet/Cargo.lock\n").is_ok());
    }

    #[test]
    fn failed_cleanup_preserves_the_original_preparation_error() {
        let dir = scratch("cleanup-error");
        let stage = dir.join("stage");
        std::fs::write(&stage, b"unexpected state").unwrap();
        let error = finish_staging(Err("checksum mismatch".into()), &stage).unwrap_err();
        assert!(error.starts_with("checksum mismatch; clean failed vendor staging:"));
        assert!(error.contains("not a real directory"));
        assert!(stage.is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_completion_marker_does_not_allow_corrupt_private_bytes_to_skip_verification() {
        let dir = scratch("corrupt-complete");
        let bytes = b"verified package";
        let job = local(&dir, bytes);
        let cache = vendor(&dir);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("dep-1.0.0.crate"), b"damaged package").unwrap();
        let digest = lock_digest(&dir.join(&job.lock)).unwrap();
        std::fs::write(cache.join(".warm-complete"), format!("{digest}\n1\n")).unwrap();
        assert!(is_warm_complete(&cache, &dir.join(&job.lock)));
        assert!(consume_job(&dir, &job, None, "http://127.0.0.1:1", &dir.join("sources")).is_err());
        assert_eq!(
            std::fs::read(cache.join("dep-1.0.0.crate")).unwrap(),
            b"damaged package"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn two_private_vendor_sets_reuse_cached_bytes_and_keep_old_state_on_failure() {
        let dir = scratch("two-consumers");
        let store = dir.join("store");
        let bytes = b"verified package";
        host_archive(&dir.join("host"), &store, bytes);
        let feed = ConsumerServer::start(store, None);
        for guest in ["one", "two"] {
            let root = dir.join(guest);
            let job = local(&root, bytes);
            consume_job(&root, &job, None, &feed.base, &root.join("sources")).unwrap();
            // This is the same applet operation used by the builder prelude.
            // A prepared cache skips producer fetching and its proxy work tree.
            warm_crate_local(&root, "net", "td-net");
            assert!(!root
                .join(".td-build-cache/crate-vendor/td-net.work")
                .exists());
            assert_eq!(
                std::fs::read(vendor(&root).join("dep-1.0.0.crate")).unwrap(),
                bytes
            );
            assert!(is_warm_complete(
                &vendor(&root),
                &root.join("net/Cargo.lock")
            ));
        }
        assert_eq!(feed.count(), 2);
        let root = dir.join("one");
        let before = std::fs::read(vendor(&root).join(".warm-complete")).unwrap();
        let missing = local(&root, b"missing package");
        assert!(consume_job(&root, &missing, None, &feed.base, &root.join("sources")).is_err());
        assert_eq!(
            std::fs::read(vendor(&root).join("dep-1.0.0.crate")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read(vendor(&root).join(".warm-complete")).unwrap(),
            before
        );
        assert!(!root
            .join(".td-build-cache/crate-vendor/.td-net.feed-new")
            .exists());
        assert_eq!(
            std::fs::read(vendor(&dir.join("two")).join("dep-1.0.0.crate")).unwrap(),
            bytes
        );
        drop(feed);
        let job = local(&root, bytes);
        consume_job(
            &root,
            &job,
            None,
            "http://127.0.0.1:1",
            &root.join("sources"),
        )
        .unwrap();
        assert!(is_warm_complete(
            &vendor(&root),
            &root.join("net/Cargo.lock")
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn producers_and_consumers_share_one_bounded_preparation_lock() {
        let dir = scratch("operation-lock");
        let guard = lock_job(&dir, "td-net").unwrap();
        assert!(lock_job_before(&dir, "td-net", Instant::now())
            .unwrap_err()
            .contains("busy"));
        let sibling = lock_job(&dir, "other").unwrap();
        drop(sibling);
        drop(guard);
        drop(lock_job_before(&dir, "td-net", Instant::now() + Duration::from_secs(2)).unwrap());
        assert!(lock_job(&dir, "../outside").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_lock_changed_during_transfer_cannot_publish_a_completion_marker() {
        let dir = scratch("lock-change");
        let root = dir.join("guest");
        let job = local(&root, b"first");
        let store = dir.join("store");
        host_archive(&dir.join("host"), &store, b"first");
        let feed = ConsumerServer::start(store.clone(), None);
        consume_job(&root, &job, None, &feed.base, &root.join("sources")).unwrap();
        drop(feed);
        let old_marker = std::fs::read(vendor(&root).join(".warm-complete")).unwrap();
        local(&root, b"second");
        host_archive(&dir.join("host"), &store, b"second");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let changed_lock = root.join("net/Cargo.lock");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        std::fs::write(changed_lock, lock_text(b"third")).unwrap();
                        super::super::handle_conn(stream, &store).unwrap();
                        break;
                    }
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("fixture request did not arrive: {error}"),
                }
            }
        });
        let error = consume_job(&root, &job, None, &base, &root.join("sources")).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("changed during"), "{error}");
        assert_eq!(
            std::fs::read(vendor(&root).join(".warm-complete")).unwrap(),
            old_marker
        );
        assert_eq!(
            std::fs::read(vendor(&root).join("dep-1.0.0.crate")).unwrap(),
            b"first"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_directory_publication_restores_previous_or_keeps_published_state() {
        let dir = scratch("recover");
        let current = dir.join("current");
        let stage = dir.join("new");
        let previous = dir.join("old");
        std::fs::create_dir(&previous).unwrap();
        std::fs::write(previous.join("value"), b"previous").unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("value"), b"partial").unwrap();
        recover(&current, &stage, &previous).unwrap();
        assert_eq!(std::fs::read(current.join("value")).unwrap(), b"previous");
        assert!(!stage.exists());
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("value"), b"complete").unwrap();
        publish(&current, &stage, &previous).unwrap();
        assert_eq!(std::fs::read(current.join("value")).unwrap(), b"complete");
        std::fs::create_dir(&previous).unwrap();
        recover(&current, &stage, &previous).unwrap();
        assert_eq!(std::fs::read(current.join("value")).unwrap(), b"complete");
        std::os::unix::fs::symlink(&current, &previous).unwrap();
        assert!(recover(&current, &stage, &previous).is_err());
        assert_eq!(std::fs::read(current.join("value")).unwrap(), b"complete");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn package_preparation_materializes_only_metadata_and_retains_the_archive() {
        let dir = scratch("metadata-only");
        let package = "fixture-1.0.0";
        let source = dir.join("src").join(package);
        let archive = dir.join("work/fixture-1.0.0.crate");
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        let lock = lock_text(b"dependency");
        let mut tar = Vec::new();
        for (name, bytes) in [
            ("Cargo.lock", lock.as_bytes()),
            ("Cargo.toml", b"[package]\nname = \"fixture\"\nversion = \"1.0.0\"\n".as_slice()),
            ("unselected-source", b"do not materialize".as_slice()),
        ] {
            let start = tar.len();
            metadata::tests::entry(&mut tar, &format!("{package}/{name}"), b'0', bytes);
            let header = &mut tar[start..start + 512];
            header[100..108].copy_from_slice(b"0000644\0");
            header[148..156].fill(b' ');
            let sum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        }
        tar.resize(tar.len() + 1024, 0);
        let original = metadata::tests::gzip(&tar);
        std::fs::write(&archive, &original).unwrap();
        let checksum = hex_sha256(&original);
        let selected = prepare_package_metadata(&archive, &checksum, package, &source).unwrap();
        assert_eq!(std::fs::read_to_string(selected).unwrap(), lock);
        assert_eq!(std::fs::read(&archive).unwrap(), original);
        assert!(!source.join("unselected-source").exists());
        assert!(std::fs::read_to_string(source.join("Cargo.toml")).unwrap().contains("[workspace]"));
        let refused = dir.join("refused");
        assert!(prepare_package_metadata(&archive, &"0".repeat(64), package, &refused).is_err());
        assert!(!refused.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn packaged_source_uses_its_pinned_archive_and_shipped_lock_without_registry_access() {
        let dir = scratch("packaged");
        let source = dir.join("source/fixture-1.0.0");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(source.join("Cargo.lock"), lock_text(b"dependency")).unwrap();
        std::fs::write(
            source.join("not-preparation-state"),
            b"build extracts this later",
        )
        .unwrap();
        let archive = dir.join("fixture-1.0.0.crate");
        let mut tar = Vec::new();
        for member in ["Cargo.lock", "Cargo.toml", "not-preparation-state"] {
            metadata::tests::entry(
                &mut tar,
                &format!("fixture-1.0.0/{member}"),
                b'0',
                &std::fs::read(source.join(member)).unwrap(),
            );
        }
        tar.resize(tar.len() + 1024, 0);
        std::fs::write(&archive, metadata::tests::gzip(&tar)).unwrap();
        let pin = SourcePin {
            key: "fixture-source".into(),
            file: "fixture-1.0.0.crate".into(),
            url: "https://example.invalid/fixture-1.0.0.crate".into(),
            sha256: file_sha256(&archive).unwrap(),
        };
        let store = dir.join("store");
        export_source_pin(&pin, &dir, &store).unwrap();
        host_archive(&dir.join("host"), &store, b"dependency");
        let feed = ConsumerServer::start(store, None);
        let job = Job {
            dest: "fixture".into(),
            lock: ".td-build-cache/crate-vendor/fixture/src/fixture-1.0.0/Cargo.lock".into(),
            source: Source::Crate {
                name: "fixture".into(),
                version: "1.0.0".into(),
            },
        };
        let root = dir.join("guest");
        let matched = source_pin(&job, std::slice::from_ref(&pin)).unwrap();
        consume_job(&root, &job, matched, &feed.base, &root.join("sources")).unwrap();
        let cv = root.join(".td-build-cache/crate-vendor/fixture");
        assert_eq!(
            file_sha256(&cv.join("work/fixture-1.0.0.crate")).unwrap(),
            pin.sha256
        );
        assert!(is_warm_complete(&cv.join("vendor"), &root.join(&job.lock)));
        assert_eq!(
            std::fs::read(cv.join("vendor/dep-1.0.0.crate")).unwrap(),
            b"dependency"
        );
        assert!(!cv.join("src/fixture-1.0.0/not-preparation-state").exists());
        assert_eq!(feed.count(), 2);
        drop(feed);
        consume_job(
            &root,
            &job,
            matched,
            "http://127.0.0.1:1",
            &root.join("sources"),
        )
        .unwrap();
        assert!(is_warm_complete(&cv.join("vendor"), &root.join(&job.lock)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
