use super::*;
use format::{Object, Payload, DAY, FILES, RECORD_LIMIT};
use std::collections::BTreeSet;
use std::io::BufReader;
use td_engine::image_chunks::{Chunker, MAX};

pub(super) fn root(
    version: u64,
    expires: u64,
    private: &Path,
    release_public: &Path,
    previous_private: &Path,
    output: &Path,
) -> Result<()> {
    let private = read(private, SMALL)?;
    let root_public = sig::public_key(&private)?;
    let public = text(release_public, SMALL)?;
    let public = format::hex(public.trim_end_matches('\n'), 32)?;
    let body = format!(
        "td-release-root-v1\n{version}\n{expires}\n{}\n{}\n",
        sig::to_hex(&root_public),
        sig::to_hex(&public)
    );
    let old = format::signed(&body, &read(previous_private, SMALL)?)?;
    let text = format::signed(&old, &private)?;
    Root::parse(&text, None)?.fresh(now()?)?;
    let mut file = io(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output))?;
    io(file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all()))
}

pub(super) fn immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    match read(path, bytes.len() as u64) {
        Ok(old) if old == bytes => Ok(()),
        Ok(_) => Err(format!(
            "immutable repository file differs: {}",
            path.display()
        )),
        Err(_) if !io(path.try_exists())? => replace(path, bytes),
        Err(error) => Err(error),
    }
}

pub(super) fn prepare(repo: &Path, root_path: &Path) -> Result<(File, Root)> {
    directory(repo)?;
    let lock = lock(&repo.join("lock"))?;
    for sub in ["roots", "heads", "releases", "objects"] {
        directory(&repo.join(sub))?;
    }
    let root = Root::parse(&text(root_path, SMALL)?, None)?;
    root.fresh(now()?)?;
    if root.version > 1 {
        let old = Root::parse(
            &text(&repo.join(format!("roots/{}", root.version - 1)), SMALL)?,
            None,
        )?;
        Root::parse(&root.text, Some(&old))?;
    }
    immutable(
        &repo.join(format!("roots/{}", root.version)),
        root.text.as_bytes(),
    )?;
    Ok((lock, root))
}

fn signing_key(path: &Path, root: &Root) -> Result<Vec<u8>> {
    let key = read(path, SMALL)?;
    let probe = b"td-release-key-check-v1";
    if !sig::verify_msg(&root.release_key, probe, &sig::sign_msg(&key, probe)?) {
        return Err("release private key does not match root policy".into());
    }
    Ok(key)
}

fn object(repo: &Path, bytes: &[u8]) -> Result<Object> {
    let digest = hash(bytes);
    immutable(&repo.join("objects").join(&digest), bytes)?;
    Ok(Object {
        size: bytes.len() as u64,
        digest,
    })
}

fn partition(path: &Path, size: u64) -> Result<std::collections::VecDeque<usize>> {
    let text = text(path, INDEX_LIMIT)?;
    if !text.ends_with('\n') {
        return Err("unterminated object partition".into());
    }
    let mut lengths = std::collections::VecDeque::new();
    let mut total = 0;
    for line in text.lines() {
        let length = format::number(line)?;
        if length == 0 || length > MAX as u64 || lengths.len() >= RECORD_LIMIT {
            return Err("object partition exceeds bounds".into());
        }
        total += length;
        lengths.push_back(length as usize);
    }
    if total != size {
        return Err("object partition does not cover payload".into());
    }
    Ok(lengths)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn publish(
    repo: &Path,
    root_path: &Path,
    key_path: &Path,
    profile: &str,
    commit: &str,
    sequence: u64,
    issued: u64,
    expires: u64,
    deployment: &Path,
) -> Result<()> {
    publish_with_partitions(
        repo, root_path, key_path, profile, commit, sequence, issued, expires, deployment, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn publish_with_partitions(
    repo: &Path,
    root_path: &Path,
    key_path: &Path,
    profile: &str,
    commit: &str,
    sequence: u64,
    issued: u64,
    expires: u64,
    deployment: &Path,
    partitions: Option<&Path>,
) -> Result<()> {
    format::profile(profile)?;
    format::hex(commit, 20)?;
    format::lifetime(issued, expires, now()?, 30 * DAY)?;
    let (_lock, root) = prepare(repo, root_path)?;
    let key = signing_key(key_path, &root)?;
    let digest = publish_locked(
        repo, &root, &key, profile, commit, sequence, issued, expires, deployment, partitions,
    )?;
    println!("{digest}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn publish_locked(
    repo: &Path,
    root: &Root,
    key: &[u8],
    profile: &str,
    commit: &str,
    sequence: u64,
    issued: u64,
    expires: u64,
    deployment: &Path,
    partitions: Option<&Path>,
) -> Result<String> {
    let manifest = text(&deployment.join("manifest"), 4096)?;
    let mut payloads = Vec::new();
    let mut count = 0usize;
    for (name, maximum) in FILES {
        let (file, meta) = io(td_fs::open_real_file(
            &deployment.join(name),
            "published payload",
        ))?;
        if meta.len() == 0 || meta.len() > maximum {
            return Err("published payload size exceeds bound".into());
        }
        let mut plan = partitions
            .map(|directory| partition(&directory.join(name), meta.len()))
            .transpose()?;
        let mut reader = BufReader::new(file);
        let mut chunker = Chunker::default();
        let mut chunk = Vec::with_capacity(MAX);
        let mut block = vec![0u8; MAX];
        let mut hasher = sha256::Sha256::new();
        let mut size = 0u64;
        let mut objects = Vec::new();
        loop {
            let length = io(reader.read(&mut block))?;
            if length == 0 {
                break;
            }
            size += length as u64;
            if size > meta.len() {
                return Err("published payload grew".into());
            }
            let data = block.get(..length).ok_or("read length")?;
            hasher.update(data);
            for byte in data {
                chunk.push(*byte);
                let cut = if let Some(plan) = &mut plan {
                    if plan.front() == Some(&chunk.len()) {
                        plan.pop_front();
                        true
                    } else {
                        false
                    }
                } else {
                    chunker.push(*byte)
                };
                if cut {
                    count += 1;
                    if count > RECORD_LIMIT {
                        return Err("too many release objects".into());
                    }
                    objects.push(object(repo, &chunk)?);
                    chunk.clear();
                }
            }
        }
        if !chunk.is_empty() {
            count += 1;
            if count > RECORD_LIMIT {
                return Err("too many release objects".into());
            }
            objects.push(object(repo, &chunk)?);
        }
        if size != meta.len() {
            return Err("published payload shrank".into());
        }
        payloads.push(Payload {
            name,
            size,
            digest: sig::to_hex(&hasher.finalize()),
            objects,
        });
    }
    if format::manifest_for(&payloads) != manifest {
        return Err("source deployment manifest does not match payloads".into());
    }
    let mut body = format!(
        "td-release-image-v1\n{}\n{sequence}\n{issued}\n{expires}\n{profile}\n{commit}\n{}\n",
        root.version,
        sig::to_hex(manifest.as_bytes())
    );
    for payload in payloads {
        body.push_str(&format!(
            "file {} {} {} {}\n",
            payload.name,
            payload.size,
            payload.digest,
            payload.objects.len()
        ));
        for object in payload.objects {
            body.push_str(&format!("{} {}\n", object.size, object.digest));
        }
    }
    let signed = format::signed(&body, key)?;
    Release::parse(&signed, root, now()?, profile)?;
    let digest = hash(signed.as_bytes());
    immutable(&repo.join("releases").join(&digest), signed.as_bytes())?;
    Ok(digest)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn promote(
    repo: &Path,
    root_path: &Path,
    key_path: &Path,
    profile: &str,
    release_digest: &str,
    version: u64,
    issued: u64,
    expires: u64,
) -> Result<()> {
    format::profile(profile)?;
    format::hex(release_digest, 32)?;
    format::lifetime(issued, expires, now()?, DAY)?;
    let (_lock, root) = prepare(repo, root_path)?;
    let key = signing_key(key_path, &root)?;
    promote_locked(
        repo,
        &root,
        &key,
        profile,
        release_digest,
        version,
        issued,
        expires,
    )
}

#[allow(clippy::too_many_arguments)]
fn promote_locked(
    repo: &Path,
    root: &Root,
    key: &[u8],
    profile: &str,
    release_digest: &str,
    version: u64,
    issued: u64,
    expires: u64,
) -> Result<()> {
    let release_text = text(&repo.join("releases").join(release_digest), INDEX_LIMIT)?;
    if hash(release_text.as_bytes()) != release_digest {
        return Err("stored release digest mismatch".into());
    }
    let release = Release::parse(&release_text, root, now()?, profile)?;
    if expires > release.expires {
        return Err("head expires after release".into());
    }
    // Check all referenced objects before making the release discoverable.
    for payload in &release.payloads {
        for object in &payload.objects {
            let bytes = read(&repo.join("objects").join(&object.digest), object.size)?;
            if bytes.len() as u64 != object.size || hash(&bytes) != object.digest {
                return Err("published object corrupt or missing".into());
            }
        }
    }
    let destination = repo.join("heads").join(profile);
    if io(destination.try_exists())? {
        let old_text = text(&destination, SMALL)?;
        let old_root_version = Head::root_hint(&old_text)?;
        if old_root_version > root.version {
            return Err("publisher root rollback".into());
        }
        let old_root = Root::parse(
            &text(&repo.join(format!("roots/{old_root_version}")), SMALL)?,
            None,
        )?;
        // An expired head can be refreshed; its signed issued time permits parsing.
        let old_issued = format::number(old_text.lines().nth(3).ok_or("old head truncated")?)?;
        let old = Head::parse(&old_text, &old_root, old_issued, profile)?;
        let old_release_text = text(&repo.join("releases").join(&old.release), INDEX_LIMIT)?;
        let old_release = Release::parse(
            &old_release_text,
            &old_root,
            format::number(
                old_release_text
                    .lines()
                    .nth(3)
                    .ok_or("old release truncated")?,
            )?,
            profile,
        )?;
        if old_root_version == root.version
            && (version <= old.version
                || release.sequence < old_release.sequence
                || (release.sequence == old_release.sequence && release_digest != old.release))
        {
            return Err("publisher head or release rollback/equivocation".into());
        }
    }
    let body = format!(
        "td-release-head-v1\n{}\n{version}\n{issued}\n{expires}\n{profile}\n{release_digest}\n",
        root.version
    );
    let signed = format::signed(&body, key)?;
    Head::parse(&signed, root, now()?, profile)?;
    replace(&destination, signed.as_bytes())
}

fn selected(repo: &Path, root: &Root, profile: &str) -> Result<Option<Head>> {
    let path = repo.join("heads").join(profile);
    if !io(path.try_exists())? {
        return Ok(None);
    }
    let head_text = text(&path, SMALL)?;
    let epoch = Head::root_hint(&head_text)?;
    if epoch > root.version {
        return Err("publisher root rollback".into());
    }
    let old_root = Root::parse(&text(&repo.join(format!("roots/{epoch}")), SMALL)?, None)?;
    let issued = format::number(head_text.lines().nth(3).ok_or("head truncated")?)?;
    Head::parse(&head_text, &old_root, issued, profile).map(Some)
}

pub(super) fn publish_next(
    repo: &Path,
    root_path: &Path,
    key_path: &Path,
    profile: &str,
    commit: &str,
    deployment: &Path,
) -> Result<()> {
    format::profile(profile)?;
    format::hex(commit, 20)?;
    let (_lock, root) = prepare(repo, root_path)?;
    let key = signing_key(key_path, &root)?;
    selected(repo, &root, profile)?;
    // Include staged indexes, so a retry never equivocates at their sequence.
    let mut sequence = 0;
    let deadline = Instant::now() + Duration::from_secs(3600);
    for (count, entry) in io(fs::read_dir(repo.join("releases")))?.enumerate() {
        if count >= 200_000 || Instant::now() >= deadline {
            return Err("release index scan exceeded its entry or time bound".into());
        }
        let entry = io(entry)?;
        let digest = entry
            .file_name()
            .into_string()
            .map_err(|_| "release name UTF-8")?;
        if digest_temporary(&digest) {
            let metadata = io(fs::symlink_metadata(entry.path()))?;
            if !metadata.is_file() && !metadata.is_symlink() {
                return Err("non-file interrupted release index".into());
            }
            io(fs::remove_file(entry.path()))?;
            continue;
        }
        format::hex(&digest, 32)?;
        let value = text(&entry.path(), INDEX_LIMIT)?;
        if hash(value.as_bytes()) != digest {
            return Err("stored release digest mismatch".into());
        }
        let epoch = format::number(value.lines().nth(1).ok_or("release truncated")?)?;
        if epoch != root.version || value.lines().nth(5) != Some(profile) {
            continue;
        }
        let issued = format::number(value.lines().nth(3).ok_or("release truncated")?)?;
        let release = Release::parse(&value, &root, issued, profile)?;
        sequence = sequence.max(release.sequence);
    }
    let sequence = sequence
        .checked_add(1)
        .ok_or("release sequence exhausted")?;
    let issued = now()?;
    let expires = issued
        .checked_add(30 * DAY)
        .ok_or("expiry overflow")?
        .min(root.expires);
    format::lifetime(issued, expires, issued, 30 * DAY)?;
    let digest = publish_locked(
        repo, &root, &key, profile, commit, sequence, issued, expires, deployment, None,
    )?;
    println!("{digest}");
    Ok(())
}

pub(super) fn promote_next(
    repo: &Path,
    root_path: &Path,
    key_path: &Path,
    profile: &str,
    requested: Option<&str>,
) -> Result<()> {
    format::profile(profile)?;
    if let Some(digest) = requested {
        format::hex(digest, 32)?;
    }
    let (_lock, root) = prepare(repo, root_path)?;
    let key = signing_key(key_path, &root)?;
    let selected = selected(repo, &root, profile)?;
    let version = match selected.as_ref() {
        Some(head) if head.root == root.version => head
            .version
            .checked_add(1)
            .ok_or("head version exhausted")?,
        _ => 1,
    };
    let digest = match requested {
        Some(digest) => digest,
        None => {
            let head = selected.as_ref().ok_or("no selected release to renew")?;
            if head.root != root.version {
                return Err("root rotation requires publishing a new index".into());
            }
            &head.release
        }
    };
    let issued = now()?;
    let value = text(&repo.join("releases").join(digest), INDEX_LIMIT)?;
    if hash(value.as_bytes()) != digest {
        return Err("stored release digest mismatch".into());
    }
    let release = Release::parse(&value, &root, issued, profile).map_err(|error| {
        format!("cannot promote or renew: {error}; publish a fresh index when expired")
    })?;
    let expires = issued
        .checked_add(DAY)
        .ok_or("expiry overflow")?
        .min(release.expires)
        .min(root.expires);
    format::lifetime(issued, expires, issued, DAY)?;
    promote_locked(repo, &root, &key, profile, digest, version, issued, expires)?;
    println!("{digest}");
    Ok(())
}

pub(super) fn gc(repo: &Path, root_path: &Path, delete: bool) -> Result<()> {
    let (_lock, _) = prepare(repo, root_path)?;
    let obsolete = garbage(repo)?;
    for path in obsolete {
        println!(
            "{} {}",
            if delete { "delete" } else { "would-delete" },
            path.display()
        );
        if delete {
            io(fs::remove_file(path))?;
        }
    }
    if delete {
        for sub in ["releases", "objects"] {
            io(File::open(repo.join(sub)).and_then(|f| f.sync_all()))?;
        }
    }
    Ok(())
}

pub(super) fn garbage(repo: &Path) -> Result<Vec<PathBuf>> {
    let time = now()?;
    let grace = 7 * DAY;
    let mut selected = BTreeSet::new();
    for name in ["system-x86-64", "server-x86-64"] {
        let path = repo.join("heads").join(name);
        if io(path.try_exists())? {
            let text = text(&path, SMALL)?;
            let version = Head::root_hint(&text)?;
            let root = Root::parse(
                &super::text(&repo.join(format!("roots/{version}")), SMALL)?,
                None,
            )?;
            let issued = format::number(text.lines().nth(3).ok_or("head truncated")?)?;
            selected.insert(Head::parse(&text, &root, issued, name)?.release);
        }
    }
    let mut reachable = BTreeSet::new();
    let mut obsolete = Vec::new();
    for entry in io(fs::read_dir(repo.join("releases")))? {
        let entry = io(entry)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid repository entry")?;
        format::hex(&name, 32)?;
        let text = text(&entry.path(), INDEX_LIMIT)?;
        if hash(text.as_bytes()) != name {
            return Err("repository release digest mismatch".into());
        }
        let mut fields = format::Lines::new(&text)?;
        fields.expect("td-release-image-v1")?;
        let root_version = fields.num()?;
        fields.num()?;
        let issued = fields.num()?;
        fields.num()?;
        let profile = fields.next()?;
        let root = Root::parse(
            &super::text(&repo.join(format!("roots/{root_version}")), SMALL)?,
            None,
        )?;
        let release = Release::parse(&text, &root, issued, profile)?;
        if selected.remove(&name) || time.saturating_sub(release.expires) <= grace {
            for payload in release.payloads {
                for object in payload.objects {
                    reachable.insert(object.digest);
                }
            }
        } else {
            obsolete.push(entry.path());
        }
    }
    if !selected.is_empty() {
        return Err("selected release index missing; refusing collection".into());
    }
    for entry in io(fs::read_dir(repo.join("objects")))? {
        let entry = io(entry)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid object filename")?;
        format::hex(&name, 32)?;
        let meta = io(fs::symlink_metadata(entry.path()))?;
        if !meta.is_file() {
            return Err("non-file in object repository".into());
        }
        let age = SystemTime::now()
            .duration_since(io(meta.modified())?)
            .unwrap_or_default();
        if !reachable.contains(&name) && age.as_secs() > grace {
            obsolete.push(entry.path());
        }
    }
    Ok(obsolete)
}
