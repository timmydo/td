use super::*;
use crate::s3;
use std::collections::BTreeMap;

struct Plan {
    roots: Vec<Root>,
    head_text: String,
    head: Head,
    index: String,
    release: Release,
    objects: BTreeMap<String, u64>,
}
impl Plan {
    fn load(repo: &Path, root_path: &Path, profile: &str, client: &s3::Client) -> Result<Self> {
        format::profile(profile)?;
        let trusted = text(root_path, SMALL)?;
        let root = Root::parse(&trusted, None)?;
        root.fresh(now()?)?;
        if root.version > 1024 {
            return Err("upload root chain exceeds 1024 epochs".into());
        }
        let mut roots = Vec::new();
        for version in 1..=root.version {
            client.remaining()?;
            let bytes = text(&repo.join(format!("roots/{version}")), SMALL)?;
            let next = Root::parse(&bytes, roots.last())?;
            if next.version != version {
                return Err("repository root version mismatch".into());
            }
            roots.push(next);
        }
        if roots.last().map(|r| r.text.as_str()) != Some(trusted.as_str()) {
            return Err("repository root does not match the supplied trust anchor".into());
        }
        let head_text = text(&repo.join("heads").join(profile), SMALL)?;
        let head = Head::parse(&head_text, &root, now()?, profile)?;
        let index = text(&repo.join("releases").join(&head.release), INDEX_LIMIT)?;
        if hash(index.as_bytes()) != head.release {
            return Err("local release index hash mismatch".into());
        }
        let release = Release::parse(&index, &root, now()?, profile)?;
        if head.expires > release.expires || release.expires > root.expires {
            return Err("release expiry exceeds signing policy".into());
        }
        let mut objects = BTreeMap::new();
        for payload in &release.payloads {
            let mut payload_hash = sha256::Sha256::new();
            for object in &payload.objects {
                client.remaining()?;
                let bytes = read(&repo.join("objects").join(&object.digest), object.size)?;
                if bytes.len() as u64 != object.size || hash(&bytes) != object.digest {
                    return Err("local release object corrupt or missing".into());
                }
                payload_hash.update(&bytes);
                if objects
                    .insert(object.digest.clone(), object.size)
                    .is_some_and(|old| old != object.size)
                {
                    return Err("inconsistent object length".into());
                }
            }
            if sig::to_hex(&payload_hash.finalize()) != payload.digest {
                return Err("local payload hash mismatch".into());
            }
        }
        Ok(Self {
            roots,
            head_text,
            head,
            index,
            release,
            objects,
        })
    }
    fn root(&self, version: u64) -> Result<&Root> {
        self.roots
            .iter()
            .find(|r| r.version == version)
            .ok_or_else(|| "remote root is newer or absent from the trusted local chain".into())
    }
    fn check_remote(&self, client: &s3::Client, remote: &s3::Remote, profile: &str) -> Result<()> {
        let old_text =
            std::str::from_utf8(&remote.bytes).map_err(|_| "remote head is not UTF-8")?;
        let root = self.root(Head::root_hint(old_text)?)?;
        let old = Head::parse(old_text, root, issued(old_text)?, profile)?;
        if old_text == self.head_text {
            return Ok(());
        }
        if old.root == self.head.root && old.version >= self.head.version {
            return Err("remote head is newer or equivocal; refusing rollback".into());
        }
        let old_index = client
            .get(&format!("releases/{}", old.release), INDEX_LIMIT)?
            .ok_or("remote head references a missing index")?;
        if hash(&old_index.bytes) != old.release {
            return Err("remote release index hash mismatch".into());
        }
        let old_index =
            std::str::from_utf8(&old_index.bytes).map_err(|_| "remote index is not UTF-8")?;
        let old_release = Release::parse(old_index, root, issued(old_index)?, profile)?;
        if old.root == self.head.root
            && (old_release.sequence > self.release.sequence
                || (old_release.sequence == self.release.sequence
                    && old.release != self.head.release))
        {
            return Err("remote release is newer or equivocal; refusing rollback".into());
        }
        Ok(())
    }
}
fn issued(text: &str) -> Result<u64> {
    format::number(text.lines().nth(3).ok_or("truncated signed metadata")?)
}

fn upload_file(
    client: &mut s3::Client,
    repo: &Path,
    key: &str,
    size: u64,
    digest: &str,
) -> Result<()> {
    let (mut file, meta) = io(td_fs::open_real_file(&repo.join(key), "release upload"))?;
    if meta.len() != size || s3::file_hash(&mut file, size, client)? != digest {
        return Err("release file changed after validation".into());
    }
    client.immutable(key, &mut file, size, digest)
}

pub(super) fn command(words: &[&str]) -> Result<()> {
    let (words, dry) = if words.last() == Some(&"--dry-run") {
        (words.get(..words.len() - 1).ok_or(HELP)?, true)
    } else {
        (words, false)
    };
    let [repo, root, profile, endpoint, region, bucket, credentials, credential_profile] = words
    else {
        return Err(HELP.into());
    };
    let repo = Path::new(repo);
    // Never walk the repository: only authenticated references become upload keys.
    for sub in ["", "roots", "heads", "releases", "objects"] {
        let path = repo.join(sub);
        io(fs::symlink_metadata(&path))?;
        directory(&path)?;
    }
    let _lock = lock(&repo.join("lock"))?;
    let mut client = s3::Client::new(
        endpoint,
        region,
        bucket,
        Path::new(credentials),
        credential_profile,
    )?;
    let plan = Plan::load(repo, Path::new(root), profile, &client)?;
    println!(
        "release S3 {}: {} objects, {} roots, index {}, head {profile} version {}",
        if dry { "dry run" } else { "upload" },
        plan.objects.len(),
        plan.roots.len(),
        plan.head.release,
        plan.head.version
    );
    if dry {
        return Ok(());
    }
    let key = format!("heads/{profile}");
    let previous = client.get(&key, SMALL)?;
    let mut protect_retention =
        plan.root(plan.head.root)?.expires == 0 || publisher::retention_hold(repo)?.is_some();
    if let Some(remote) = &previous {
        plan.check_remote(&client, remote, profile)?;
        let old = std::str::from_utf8(&remote.bytes).map_err(|_| "remote head is not UTF-8")?;
        protect_retention |= plan.root(Head::root_hint(old)?)?.expires == 0;
    }
    for (digest, size) in &plan.objects {
        upload_file(
            &mut client,
            repo,
            &format!("objects/{digest}"),
            *size,
            digest,
        )?;
    }
    for root in &plan.roots {
        upload_file(
            &mut client,
            repo,
            &format!("roots/{}", root.version),
            root.text.len() as u64,
            &hash(root.text.as_bytes()),
        )?;
    }
    upload_file(
        &mut client,
        repo,
        &format!("releases/{}", plan.head.release),
        plan.index.len() as u64,
        &plan.head.release,
    )?;
    let root = plan.root(plan.head.root)?;
    Head::parse(&plan.head_text, root, now()?, profile)?;
    Release::parse(&plan.index, root, now()?, profile)?;
    let (mut file, meta) = io(td_fs::open_real_file(&repo.join(&key), "release head"))?;
    if meta.len() != plan.head_text.len() as u64
        || s3::file_hash(&mut file, meta.len(), &client)? != hash(plan.head_text.as_bytes())
    {
        return Err("local head changed during upload".into());
    }
    // Persist before the remote write, including an ambiguous successful PUT.
    // A delayed upload must not inherit the earlier local promotion's cutoff.
    if protect_retention {
        publisher::hold_retention(repo)?;
    }
    client.head(
        &key,
        &mut file,
        plan.head_text.as_bytes(),
        previous.as_ref(),
    )?;
    println!(
        "release S3 complete: {} immutable files uploaded, {} reused; head published last",
        client.uploaded, client.reused
    );
    Ok(())
}
