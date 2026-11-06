//! Signed static system-image delivery. No installer or publisher code is executed.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

mod cache;
mod format;
mod publisher;
mod seed;
#[cfg(test)]
mod tests;
mod upload;

use crate::{sig, td_fs, td_fs_replace};
use format::{Head, Release, Root, State, INDEX_LIMIT, SMALL};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use td_engine::sha256;

type Result<T> = std::result::Result<T, String>;
fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|e| e.to_string())
}
fn hash(bytes: &[u8]) -> String {
    sha256::hex_digest(bytes)
}
fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| e.to_string())
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    io(td_fs::read_bounded_real_file(path, "release file", limit))
}
fn text(path: &Path, limit: u64) -> Result<String> {
    String::from_utf8(read(path, limit)?).map_err(|_| "release metadata is not UTF-8".into())
}
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    io(td_fs_replace::replace(path, bytes, 0o600))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    let archive = sidecar(path, ".roots");
    directory(&archive)?;
    let destination = archive.join(state.root.version.to_string());
    publisher::immutable(&destination, state.root.text.as_bytes())?;
    replace(path, state.text().as_bytes())
}

fn directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", path.display())),
    }
    let meta = io(fs::symlink_metadata(path))?;
    let owner = io(fs::metadata("/proc/self"))?.uid();
    if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} must be an owned protected directory",
            path.display()
        ));
    }
    Ok(())
}

fn digest_temporary(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let mut fields = rest.split('.');
    let (Some(digest), Some(pid), Some(serial), Some("tmp"), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return false;
    };
    format::hex(digest, 32).is_ok() && format::number(pid).is_ok() && format::number(serial).is_ok()
}

fn lock(path: &Path) -> Result<File> {
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => io(file.sync_all())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    let (file, meta) = io(td_fs::open_real_file(path, "release lock"))?;
    if meta.uid() != io(fs::metadata("/proc/self"))?.uid() || meta.mode() & 0o022 != 0 {
        return Err("release lock is not protected".into());
    }
    file.try_lock()
        .map_err(|e| format!("release operation already active: {e}"))?;
    Ok(file)
}

struct Source {
    base: String,
    agent: Option<ureq::Agent>,
    deadline: Instant,
    requests: u64,
    bytes: u64,
}
impl Source {
    fn new(base: &str) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(3600);
        let base = base.trim_end_matches('/').to_string();
        let agent = if let Some(local) = base.strip_prefix("file:") {
            if !Path::new(local).is_absolute() {
                return Err("local repository must be absolute".into());
            }
            None
        } else {
            let uri: ureq::http::Uri = base.parse().map_err(|_| "invalid repository URL")?;
            let loopback = matches!(uri.host(), Some("127.0.0.1") | Some("[::1]"));
            if !(uri.scheme_str() == Some("https")
                || (uri.scheme_str() == Some("http") && loopback))
                || uri.authority().is_none()
                || base.contains(['@', '?', '#', '\\'])
            {
                return Err(
                    "repository requires HTTPS, loopback HTTP or file:/absolute/path".into(),
                );
            }
            Some(crate::http::agent_before(deadline, false)?)
        };
        Ok(Self {
            base,
            agent,
            deadline,
            requests: 0,
            bytes: 0,
        })
    }
    fn get(&mut self, relative: &str, maximum: u64) -> Result<Vec<u8>> {
        if Instant::now() >= self.deadline {
            return Err("release download exceeded one hour".into());
        }
        // Only compiled names, decimal versions and validated hashes reach this method.
        let bytes = if let Some(agent) = &self.agent {
            self.requests += 1;
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .ok_or("release download deadline")?;
            let mut response = agent
                .get(format!("{}/{relative}", self.base))
                .config()
                .timeout_global(Some(remaining))
                .build()
                .call()
                .map_err(|e| e.to_string())?;
            if response.status().as_u16() != 200 {
                return Err(format!("repository {relative}: HTTP {}", response.status()));
            }
            let mut bytes = Vec::new();
            io(response
                .body_mut()
                .as_reader()
                .take(maximum + 1)
                .read_to_end(&mut bytes))?;
            if bytes.len() as u64 > maximum {
                return Err("repository response exceeds bound".into());
            }
            bytes
        } else {
            read(
                &Path::new(self.base.strip_prefix("file:").ok_or("local repository")?)
                    .join(relative),
                maximum,
            )?
        };
        self.bytes += bytes.len() as u64;
        Ok(bytes)
    }
    fn text(&mut self, relative: &str, maximum: u64) -> Result<String> {
        String::from_utf8(self.get(relative, maximum)?)
            .map_err(|_| "repository metadata is not UTF-8".into())
    }
}

fn advance_roots(
    state: &mut State,
    target: u64,
    mut get: impl FnMut(u64) -> Result<String>,
    mut persist: impl FnMut(&State) -> Result<()>,
) -> Result<()> {
    if target < state.root.version || target - state.root.version > 64 {
        return Err(
            "root rollback or more than 64 rotations; provision a current trusted root".into(),
        );
    }
    while state.root.version < target {
        let next = state
            .root
            .version
            .checked_add(1)
            .ok_or("root version overflow")?;
        state.rotate(&get(next)?)?;
        // A learned revocation survives even if the later image download fails.
        persist(state)?;
    }
    Ok(())
}

fn metadata(state: &mut State, head_text: &str, release_text: &str, time: u64) -> Result<Release> {
    state.clock(time)?;
    let head = Head::parse(head_text, &state.root, time, &state.profile)?;
    if hash(release_text.as_bytes()) != head.release {
        return Err("release metadata hash mismatch".into());
    }
    let release = Release::parse(release_text, &state.root, time, &state.profile)?;
    state.accept(&head, head_text, &release, time)?;
    Ok(release)
}

/// Learn authenticated floors even when payloads or later metadata are absent.
/// Untrusted evidence can be incomplete. Protected-state I/O must still fail closed.
fn learn(state_path: &Path, evidence: &Path) -> Result<()> {
    let _lock = lock(&sidecar(state_path, ".lock"))?;
    let mut state = State::parse(&text(state_path, SMALL * 3)?)?;
    let time = now()?;
    state.clock(time)?;
    let Ok(head_text) = text(&evidence.join("head"), SMALL) else {
        return Ok(());
    };
    let Ok(target) = Head::root_hint(&head_text) else {
        return Ok(());
    };
    if target < state.root.version || target - state.root.version > 64 {
        return Ok(());
    }
    while state.root.version < target {
        let version = state
            .root
            .version
            .checked_add(1)
            .ok_or("root version overflow")?;
        let Ok(root) = text(&evidence.join(format!("root-{version}")), SMALL) else {
            return Ok(());
        };
        if state.rotate(&root).is_err() {
            return Ok(());
        }
        save_state(state_path, &state)?;
    }
    let Ok(release_text) = text(&evidence.join("release"), INDEX_LIMIT) else {
        return Ok(());
    };
    let authenticated = (|| {
        // Expired signed evidence can raise a floor but can never authorize an
        // installation. Otherwise a crash until after expiry would lose it.
        let head_issued = format::number(head_text.lines().nth(3).ok_or("head truncated")?)?;
        let release_issued =
            format::number(release_text.lines().nth(3).ok_or("release truncated")?)?;
        if head_issued > time || release_issued > time {
            return Err("future metadata".into());
        }
        let head = Head::parse(&head_text, &state.root, head_issued, &state.profile)?;
        if hash(release_text.as_bytes()) != head.release {
            return Err("release hash mismatch".into());
        }
        let release = Release::parse(&release_text, &state.root, release_issued, &state.profile)?;
        state.accept(&head, &head_text, &release, time)
    })();
    if authenticated.is_ok() {
        save_state(state_path, &state)?;
    }
    Ok(())
}

fn verify_payloads(bundle: &Path, release: &Release) -> Result<()> {
    if text(&bundle.join("manifest"), 4096)? != release.manifest {
        return Err("downloaded manifest mismatch".into());
    }
    for payload in &release.payloads {
        let (mut file, meta) = io(td_fs::open_real_file(
            &bundle.join(payload.name),
            "release payload",
        ))?;
        if meta.len() != payload.size {
            return Err(format!("{} size mismatch", payload.name));
        }
        let mut hasher = sha256::Sha256::new();
        let mut buffer = vec![0; td_engine::image_chunks::MAX];
        let mut remaining = payload.size;
        while remaining > 0 {
            let count = io(file.read(&mut buffer))?;
            if count == 0 || count as u64 > remaining {
                return Err("payload changed or truncated".into());
            }
            hasher.update(buffer.get(..count).ok_or("payload read length")?);
            remaining -= count as u64;
        }
        if io(file.read(&mut buffer))? != 0 || sig::to_hex(&hasher.finalize()) != payload.digest {
            return Err(format!("{} SHA-256 mismatch", payload.name));
        }
    }
    Ok(())
}

fn verify(state_path: &Path, bundle: &Path) -> Result<(String, String)> {
    let _lock = lock(&sidecar(state_path, ".lock"))?;
    let mut state = State::parse(&text(state_path, SMALL * 3)?)?;
    let head = text(&bundle.join("head"), SMALL)?;
    advance_roots(
        &mut state,
        Head::root_hint(&head)?,
        |version| text(&bundle.join(format!("root-{version}")), SMALL),
        |state| save_state(state_path, state),
    )?;
    let release_text = text(&bundle.join("release"), INDEX_LIMIT)?;
    let release = metadata(&mut state, &head, &release_text, now()?)?;
    verify_payloads(bundle, &release)?;
    // Recheck expiry after hashing a potentially large image.
    metadata(&mut state, &head, &release_text, now()?)?;
    save_state(state_path, &state)?;
    Ok((release.commit, hash(release.manifest.as_bytes())))
}

fn assemble(
    payload: &format::Payload,
    cache: &mut cache::Cache,
    seed: &seed::Seed,
    source: &mut Source,
    writer: &mut impl Write,
) -> Result<(u64, u64, u64)> {
    let mut hasher = sha256::Sha256::new();
    let mut downloaded = 0;
    let mut reused = 0;
    let mut seeded = 0;
    for object in &payload.objects {
        if Instant::now() >= source.deadline {
            return Err("release download exceeded one hour".into());
        }
        let bytes = if let Some(bytes) = cache.get(&object.digest, object.size)? {
            reused += object.size;
            bytes
        } else if let Some(bytes) = seed.get(&object.digest, object.size)? {
            reused += object.size;
            seeded += object.size;
            bytes
        } else {
            let bytes = source.get(&format!("objects/{}", object.digest), object.size)?;
            if bytes.len() as u64 != object.size || hash(&bytes) != object.digest {
                return Err("downloaded object hash or length mismatch".into());
            }
            cache.put(&object.digest, &bytes)?;
            downloaded += object.size;
            bytes
        };
        hasher.update(&bytes);
        io(writer.write_all(&bytes))?;
    }
    if sig::to_hex(&hasher.finalize()) != payload.digest {
        return Err("reconstructed payload hash mismatch".into());
    }
    Ok((downloaded, reused, seeded))
}

fn fetch(base: &str, state_path: &Path, cache: &Path, output: &Path) -> Result<()> {
    fetch_seeded(base, state_path, cache, output, None)
}

fn fetch_seeded(
    base: &str,
    state_path: &Path,
    cache: &Path,
    output: &Path,
    seed: Option<&Path>,
) -> Result<()> {
    fetch_budgeted(base, state_path, cache, output, seed, cache::DEFAULT_LIMIT)
}

fn fetch_budgeted(
    base: &str,
    state_path: &Path,
    cache: &Path,
    output: &Path,
    seed: Option<&Path>,
    limit: u64,
) -> Result<()> {
    let _state_lock = lock(&sidecar(state_path, ".lock"))?;
    directory(cache)?;
    let _cache_lock = lock(&cache.join("lock"))?;
    let mut state = State::parse(&text(state_path, SMALL * 3)?)?;
    state.clock(now()?)?;
    let evidence = sidecar(state_path, ".metadata");
    directory(&evidence)?;
    let mut source = Source::new(base)?;
    let head_text = source.text(&format!("heads/{}", state.profile), SMALL)?;
    // Evidence is public signed data, not a trusted worker receipt. Keep it
    // before advancing any worker state so a drained failed job can be learned.
    replace(&evidence.join("head"), head_text.as_bytes())?;
    advance_roots(
        &mut state,
        Head::root_hint(&head_text)?,
        |version| source.text(&format!("roots/{version}"), SMALL),
        |state| {
            replace(
                &evidence.join(format!("root-{}", state.root.version)),
                state.root.text.as_bytes(),
            )?;
            save_state(state_path, state)
        },
    )?;
    let head = Head::parse(&head_text, &state.root, now()?, &state.profile)?;
    let release_text = source.text(&format!("releases/{}", head.release), INDEX_LIMIT)?;
    replace(&evidence.join("release"), release_text.as_bytes())?;
    let release = metadata(&mut state, &head_text, &release_text, now()?)?;
    // Persist freshness before acquisition, so failed downloads cannot reset the floor.
    save_state(state_path, &state)?;
    let parent = output.parent().ok_or("output has no parent")?;
    directory(parent)?;
    if output.try_exists().map_err(|e| e.to_string())? {
        return Err("output already exists".into());
    }
    let stage = sidecar(output, ".partial");
    // The output parent and this fixed child belong exclusively to this invocation.
    match fs::remove_dir_all(&stage) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let output_bytes = release.payloads.iter().try_fold(0u64, |total, payload| {
        total
            .checked_add(payload.size)
            .ok_or("output size overflow")
    })?;
    eprintln!("release disk: {output_bytes} payload output bytes, {limit} maximum cached object bytes; metadata and installer staging are additional");
    let mut cache = cache::Cache::open(cache, limit, source.deadline)?;
    let seed = seed
        .map(|deployment| seed::scan(&release, &mut cache, deployment, source.deadline))
        .transpose()?
        .unwrap_or_default();
    io(fs::DirBuilder::new().mode(0o700).create(&stage))?;
    let result = (|| {
        for version in state.root.version.saturating_sub(63).max(1)..=state.root.version {
            let archived = sidecar(state_path, ".roots").join(version.to_string());
            if io(archived.try_exists())? {
                let root = text(&archived, SMALL)?;
                replace(&stage.join(format!("root-{version}")), root.as_bytes())?;
            }
        }
        replace(&stage.join("head"), head_text.as_bytes())?;
        replace(&stage.join("release"), release_text.as_bytes())?;
        replace(&stage.join("manifest"), release.manifest.as_bytes())?;
        let mut reused = 0u64;
        let mut downloaded = 0u64;
        let mut seeded = 0u64;
        for payload in &release.payloads {
            let mut file = io(OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(stage.join(payload.name)))?;
            let (new, old, local) = assemble(payload, &mut cache, &seed, &mut source, &mut file)?;
            seeded += local;
            downloaded += new;
            reused += old;
            io(file.sync_all())?;
        }
        metadata(&mut state, &head_text, &release_text, now()?)?;
        io(File::open(&stage).and_then(|f| f.sync_all()))?;
        io(fs::rename(&stage, output))?;
        io(File::open(parent).and_then(|f| f.sync_all()))?;
        eprintln!("release seed: {seeded} verified object bytes from local deployment");
        eprintln!("release transfer: {} requests, {} response body bytes, {downloaded} new object bytes, {reused} reused output bytes", source.requests, source.bytes);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

const HELP: &str = "td-net release keygen PRIVATE PUBLIC\n  root VERSION EXPIRES ROOT_PRIVATE RELEASE_PUBLIC PREVIOUS_ROOT_PRIVATE OUTPUT\n  init STATE ROOT PROFILE\n  publish REPOSITORY ROOT RELEASE_PRIVATE PROFILE COMMIT SEQUENCE ISSUED EXPIRES DEPLOYMENT\n  publish-partitioned REPOSITORY ROOT RELEASE_PRIVATE PROFILE COMMIT SEQUENCE ISSUED EXPIRES DEPLOYMENT PARTITIONS\n  promote REPOSITORY ROOT RELEASE_PRIVATE PROFILE RELEASE_HASH VERSION ISSUED EXPIRES\n  publish-next REPOSITORY ROOT RELEASE_PRIVATE PROFILE COMMIT DEPLOYMENT\n  promote-next REPOSITORY ROOT RELEASE_PRIVATE PROFILE RELEASE_HASH\n  renew REPOSITORY ROOT RELEASE_PRIVATE PROFILE\n  fetch URL STATE CACHE OUTPUT [--seed DEPLOYMENT] [--cache-limit BYTES]\n  verify STATE BUNDLE\n  learn STATE EVIDENCE\n  gc REPOSITORY ROOT [--delete]\n  upload-s3 REPOSITORY ROOT PROFILE ENDPOINT REGION BUCKET CREDENTIALS CREDENTIAL_PROFILE [--dry-run]\n";

fn command(args: &[String]) -> Result<()> {
    let words: Vec<_> = args.iter().skip(1).map(String::as_str).collect();
    match words.as_slice() {
        ["upload-s3", rest @ ..] => upload::command(rest),
        ["keygen", private, public] => {
            let (secret, key) = sig::keygen()?;
            sig::write_keypair(private, &secret, public, &key)
        }
        ["root", version, expires, root_private, release_public, old_private, output] => {
            publisher::root(
                format::number(version)?,
                format::number(expires)?,
                Path::new(root_private),
                Path::new(release_public),
                Path::new(old_private),
                Path::new(output),
            )
        }
        ["init", state, root, profile] => {
            let state = Path::new(state);
            let _lock = lock(&sidecar(state, ".lock"))?;
            if state.try_exists().map_err(|e| e.to_string())? {
                return Err("trusted release state already exists".into());
            }
            let value = State::init(&text(Path::new(root), SMALL)?, profile, now()?)?;
            save_state(state, &value)
        }
        ["publish", repo, root, key, profile, commit, sequence, issued, expires, deployment] => {
            publisher::publish(
                Path::new(repo),
                Path::new(root),
                Path::new(key),
                profile,
                commit,
                format::number(sequence)?,
                format::number(issued)?,
                format::number(expires)?,
                Path::new(deployment),
            )
        }
        ["publish-partitioned", repo, root, key, profile, commit, sequence, issued, expires, deployment, partitions] => {
            publisher::publish_with_partitions(
                Path::new(repo),
                Path::new(root),
                Path::new(key),
                profile,
                commit,
                format::number(sequence)?,
                format::number(issued)?,
                format::number(expires)?,
                Path::new(deployment),
                Some(Path::new(partitions)),
            )
        }
        ["promote", repo, root, key, profile, release, version, issued, expires] => {
            publisher::promote(
                Path::new(repo),
                Path::new(root),
                Path::new(key),
                profile,
                release,
                format::number(version)?,
                format::number(issued)?,
                format::number(expires)?,
            )
        }
        ["fetch", base, state, cache, output] => {
            fetch(base, Path::new(state), Path::new(cache), Path::new(output))
        }
        ["fetch", base, state, cache, output, "--seed", deployment] => fetch_seeded(
            base,
            Path::new(state),
            Path::new(cache),
            Path::new(output),
            Some(Path::new(deployment)),
        ),
        ["fetch", base, state, cache, output, "--cache-limit", limit] => fetch_budgeted(
            base,
            Path::new(state),
            Path::new(cache),
            Path::new(output),
            None,
            format::number(limit)?,
        ),
        ["fetch", base, state, cache, output, "--seed", deployment, "--cache-limit", limit] => {
            fetch_budgeted(
                base,
                Path::new(state),
                Path::new(cache),
                Path::new(output),
                Some(Path::new(deployment)),
                format::number(limit)?,
            )
        }
        ["publish-next", repo, root, key, profile, commit, deployment] => publisher::publish_next(
            Path::new(repo),
            Path::new(root),
            Path::new(key),
            profile,
            commit,
            Path::new(deployment),
        ),
        ["promote-next", repo, root, key, profile, digest] => publisher::promote_next(
            Path::new(repo),
            Path::new(root),
            Path::new(key),
            profile,
            Some(digest),
        ),
        ["renew", repo, root, key, profile] => publisher::promote_next(
            Path::new(repo),
            Path::new(root),
            Path::new(key),
            profile,
            None,
        ),
        ["verify", state, bundle] => {
            let (commit, deployment) = verify(Path::new(state), Path::new(bundle))?;
            println!("{commit} {deployment}");
            Ok(())
        }
        ["learn", state, evidence] => learn(Path::new(state), Path::new(evidence)),
        ["gc", repo, root] => publisher::gc(Path::new(repo), Path::new(root), false),
        ["gc", repo, root, "--delete"] => publisher::gc(Path::new(repo), Path::new(root), true),
        _ => Err(HELP.into()),
    }
}

pub(crate) fn run(args: &[String]) {
    if let Err(error) = command(args) {
        eprintln!("td-release: {error}");
        std::process::exit(1);
    }
}
