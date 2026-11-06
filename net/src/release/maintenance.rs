//! Single-publisher maintenance; remote deletion is separate from upload.
use super::*;
use crate::s3;
use std::collections::BTreeMap;

pub(super) fn renew_upload(words: &[&str]) -> Result<()> {
    let [repo, root, key, profile, endpoint, region, bucket, credentials, credential_profile] =
        words
    else {
        return Err(HELP.into());
    };
    publisher::promote_next(
        Path::new(repo),
        Path::new(root),
        Path::new(key),
        profile,
        None,
    )?;
    upload::command(&[
        repo,
        root,
        profile,
        endpoint,
        region,
        bucket,
        credentials,
        credential_profile,
    ])
}
fn remote_heads(client: &s3::Client, repo: &Path) -> Result<BTreeMap<String, Option<Vec<u8>>>> {
    let mut heads = BTreeMap::new();
    for profile in ["system-x86-64", "server-x86-64"] {
        let key = format!("heads/{profile}");
        let remote = client.get(&key, SMALL)?.map(|r| r.bytes);
        let local = match fs::symlink_metadata(repo.join(&key)) {
            Ok(_) => Some(read(&repo.join(&key), SMALL)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        if remote != local {
            return Err(
                "remote heads differ from local repository; synchronize before cleanup".into(),
            );
        }
        heads.insert(key, remote);
    }
    Ok(heads)
}
pub(super) fn gc(words: &[&str]) -> Result<()> {
    let (words, delete) = if words.ends_with(&["--delete", "--exclusive-publisher"]) {
        (words.get(..words.len() - 2).ok_or(HELP)?, true)
    } else {
        (words, false)
    };
    let [repo, root, endpoint, region, bucket, credentials, profile] = words else {
        return Err(HELP.into());
    };
    let repo = Path::new(repo);
    let (_lease, _root) = publisher::prepare(repo, Path::new(root))?;
    let client = s3::Client::new(endpoint, region, bucket, Path::new(credentials), profile)?;
    let heads = remote_heads(&client, repo)?;
    let keys = client.release_keys()?;
    // Every remote index must be present byte-for-byte locally before local
    // reachability can authorize deleting anything. Unknown remote history refuses.
    for key in &keys {
        client.remaining()?;
        let local = read(&repo.join(key), INDEX_LIMIT)?;
        let remote = client
            .get(key, INDEX_LIMIT)?
            .ok_or("remote index disappeared during cleanup")?;
        if remote.bytes != local {
            return Err("remote release differs from local repository; refusing cleanup".into());
        }
    }
    let mut obsolete = publisher::garbage(repo)?;
    // Remove expired discovery metadata before its unreachable payloads.
    obsolete.sort_by_key(|path| path.parent() != Some(&repo.join("releases")));
    let mut plan = Vec::new();
    for path in obsolete {
        client.remaining()?;
        let relative = path
            .strip_prefix(repo)
            .map_err(|_| "cleanup path outside repository")?;
        let key = relative.to_str().ok_or("cleanup key UTF-8")?.to_string();
        let (mut file, meta) = io(td_fs::open_real_file(&path, "cleanup candidate"))?;
        let digest = s3::file_hash(&mut file, meta.len(), &client)?;
        client.prune(&key, &digest, meta.len(), false)?;
        plan.push((key, digest, meta.len()));
    }
    if remote_heads(&client, repo)? != heads || client.release_keys()? != keys {
        return Err("remote publication changed during cleanup planning".into());
    }
    println!(
        "S3 cleanup {}: {} obsolete remote/local candidates; roots, heads, ISOs and index.html retained",
        if delete { "delete" } else { "dry run" },
        plan.len()
    );
    for (key, digest, size) in plan {
        println!(
            "{} {key} {size}",
            if delete { "delete" } else { "would-delete" }
        );
        if delete {
            if remote_heads(&client, repo)? != heads {
                return Err("remote head changed during cleanup; stopping".into());
            }
            client.prune(&key, &digest, size, true)?;
            // Use this exact plan, never a later GC cutoff. A retry also reclaims
            // a local candidate whose remote deletion succeeded before a crash.
            io(fs::remove_file(repo.join(&key)))?;
        }
    }
    if delete {
        for sub in ["releases", "objects"] {
            io(File::open(repo.join(sub)).and_then(|f| f.sync_all()))?;
        }
    }
    Ok(())
}
