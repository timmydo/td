//! The bucket is the publisher's history: copy its authenticated roots,
//! indexes and heads into a local repository before numbering a release.
use super::*;
use crate::s3;

/// What to do with a local head when the bucket holds another.
#[derive(Debug, PartialEq, Eq)]
enum Choice {
    /// The same head, or a local one ahead of the bucket's: a promotion
    /// whose upload has not happened yet.
    Keep,
    /// The bucket's is newer, or the same counter with other bytes, which
    /// only another publisher's promotion makes: what clients see wins.
    Take,
}

fn choose(local: Option<(&str, &Head)>, remote: (&str, &Head)) -> Choice {
    let Some((local_text, local)) = local else {
        return Choice::Take;
    };
    if local_text == remote.0 {
        return Choice::Keep;
    }
    let remote = remote.1;
    if (remote.root, remote.version) < (local.root, local.version) {
        Choice::Keep
    } else {
        Choice::Take
    }
}

/// Roots 1 through `trusted`, each signed by its predecessor, from `local`
/// or else the bucket, with how many came from the bucket. A bucket that
/// holds an authentic next epoch refuses: this host's root is out of date.
pub(super) fn chain(
    trusted: &Root,
    client: &s3::Client,
    local: impl Fn(u64) -> Result<Option<String>>,
) -> Result<(Vec<Root>, usize)> {
    if trusted.version > 1024 {
        return Err("root chain exceeds 1024 epochs".into());
    }
    let mut chain: Vec<Root> = Vec::new();
    let mut fetched = 0;
    for version in 1..=trusted.version {
        client.remaining()?;
        let value = match local(version)? {
            Some(value) => value,
            None if version == trusted.version => trusted.text.clone(),
            None => {
                let remote = client
                    .get(&format!("roots/{version}"), SMALL)?
                    .ok_or_else(|| {
                        format!("neither this host nor the bucket holds roots/{version}")
                    })?;
                fetched += 1;
                String::from_utf8(remote.bytes).map_err(|_| "remote root is not UTF-8")?
            }
        };
        let root = Root::parse(&value, chain.last())?;
        if root.version != version {
            return Err("root version does not match its key".into());
        }
        chain.push(root);
    }
    if chain.last().map(|r| r.text.as_str()) != Some(trusted.text.as_str()) {
        return Err("root chain does not end at the configured root".into());
    }
    let next = trusted
        .version
        .checked_add(1)
        .ok_or("root version overflow")?;
    if let Some(remote) = client.get(&format!("roots/{next}"), SMALL)? {
        let rotated = std::str::from_utf8(&remote.bytes)
            .map_err(|_| "remote root is not UTF-8".to_string())
            .and_then(|value| Root::parse(value, Some(trusted)));
        return Err(match rotated {
            Ok(_) => format!(
                "the bucket holds roots/{next}, signed by this root; configure this publisher with the current root"
            ),
            Err(e) => format!("the bucket holds roots/{next}, which this root did not sign: {e}"),
        });
    }
    Ok((chain, fetched))
}

/// The repository's chain: `chain`, with every root stored locally.
fn roots(repo: &Path, trusted: &Root, client: &s3::Client) -> Result<(Vec<Root>, usize)> {
    let (chain, fetched) = chain(trusted, client, |version| {
        let path = repo.join(format!("roots/{version}"));
        if io(path.try_exists())? {
            text(&path, SMALL).map(Some)
        } else {
            Ok(None)
        }
    })?;
    for root in &chain {
        publisher::immutable(
            &repo.join(format!("roots/{}", root.version)),
            root.text.as_bytes(),
        )?;
    }
    Ok((chain, fetched))
}

pub(super) fn epoch(chain: &[Root], version: u64) -> Result<&Root> {
    chain
        .iter()
        .find(|r| r.version == version)
        .ok_or_else(|| "bucket metadata names a root this publisher does not trust".into())
}

/// The authenticated head in `value`, checked at its own issue time: an
/// old selection is history, not an admission.
fn head(chain: &[Root], value: &str, profile: &str) -> Result<Head> {
    let root = epoch(chain, Head::root_hint(value)?)?;
    Head::parse(value, root, upload::issued(value)?, profile)
}

/// Every remote index the repository lacks, authenticated under its root.
fn indexes(repo: &Path, chain: &[Root], client: &s3::Client) -> Result<(usize, usize)> {
    let keys = client.release_keys()?;
    let mut fetched = 0;
    for key in &keys {
        client.remaining()?;
        let name = key
            .strip_prefix("releases/")
            .ok_or("unexpected release key")?;
        let path = repo.join("releases").join(name);
        // Content-addressed: a present file is checked wherever it is read.
        if io(path.try_exists())? {
            continue;
        }
        let remote = client
            .get(key, INDEX_LIMIT)?
            .ok_or("remote index disappeared during pull")?;
        if hash(&remote.bytes) != name {
            return Err(format!("remote {key} does not match its digest"));
        }
        let value = std::str::from_utf8(&remote.bytes).map_err(|_| "remote index is not UTF-8")?;
        let mut fields = format::Lines::new(value)?;
        fields.expect("td-release-image-v1")?;
        let root = epoch(chain, fields.num()?)?;
        fields.num()?;
        let issued = fields.num()?;
        fields.num()?;
        let profile = fields.next()?;
        Release::parse(value, root, issued, profile)?;
        publisher::immutable(&path, &remote.bytes)?;
        fetched += 1;
    }
    Ok((keys.len(), fetched))
}

/// What pulling one profile's head does.
#[derive(Debug, PartialEq, Eq)]
enum Pulled {
    Absent,
    Unchanged {
        version: u64,
    },
    Ahead {
        version: u64,
    },
    /// The local head becomes the bucket's; `lasting` when either selection
    /// is under a non-expiring root.
    Taken {
        version: u64,
        lasting: bool,
    },
}

impl Pulled {
    fn describe(&self) -> String {
        match self {
            Self::Absent => "absent from the bucket".into(),
            Self::Unchanged { version } => format!("unchanged at version {version}"),
            Self::Ahead { version } => format!(
                "kept at version {version}, ahead of the bucket (upload pending, or the bucket was rolled back)"
            ),
            Self::Taken { version, .. } => format!("taken from the bucket at version {version}"),
        }
    }
}

/// One profile's head, authenticated, with what pulling it would write.
fn plan_head(
    repo: &Path,
    chain: &[Root],
    client: &s3::Client,
    profile: &str,
) -> Result<(Pulled, Option<String>)> {
    let key = format!("heads/{profile}");
    let Some(remote) = client.get(&key, SMALL)? else {
        return Ok((Pulled::Absent, None));
    };
    let remote_text = String::from_utf8(remote.bytes).map_err(|_| "remote head is not UTF-8")?;
    let remote_head = head(chain, &remote_text, profile)?;
    // `indexes` stored every index the bucket lists.
    if !io(repo
        .join("releases")
        .join(&remote_head.release)
        .try_exists())?
    {
        return Err(format!(
            "remote {key} names an index the bucket does not list"
        ));
    }
    let path = repo.join(&key);
    let local_text = if io(path.try_exists())? {
        Some(text(&path, SMALL)?)
    } else {
        None
    };
    let local_head = local_text
        .as_deref()
        .map(|value| head(chain, value, profile))
        .transpose()?;
    let local = local_text.as_deref().zip(local_head.as_ref());
    Ok(
        match (choose(local, (&remote_text, &remote_head)), &local_head) {
            (Choice::Keep, Some(old))
                if old.version == remote_head.version && old.root == remote_head.root =>
            {
                (
                    Pulled::Unchanged {
                        version: old.version,
                    },
                    None,
                )
            }
            (Choice::Keep, Some(old)) => (
                Pulled::Ahead {
                    version: old.version,
                },
                None,
            ),
            (Choice::Keep, None) => return Err("no local head to keep".into()),
            (Choice::Take, _) => {
                let mut lasting = epoch(chain, remote_head.root)?.expires == 0;
                if let Some(old) = &local_head {
                    lasting |= epoch(chain, old.root)?.expires == 0;
                }
                let pulled = Pulled::Taken {
                    version: remote_head.version,
                    lasting,
                };
                (pulled, Some(remote_text))
            }
        },
    )
}

/// `pull-s3 REPOSITORY ROOT ENDPOINT REGION BUCKET CREDENTIALS PROFILE`.
pub(super) fn command(words: &[&str]) -> Result<()> {
    let [repo, root, endpoint, region, bucket, credentials, credential_profile] = words else {
        return Err(HELP.into());
    };
    let repo = Path::new(repo);
    directory(repo)?;
    let _lock = lock(&repo.join("lock"))?;
    for sub in ["roots", "heads", "releases", "objects"] {
        directory(&repo.join(sub))?;
    }
    let trusted = Root::parse(&text(Path::new(root), SMALL)?, None)?;
    trusted.fresh(now()?)?;
    let client = s3::Client::new(
        endpoint,
        region,
        bucket,
        Path::new(credentials),
        credential_profile,
    )?;
    let (chain, roots_fetched) = roots(repo, &trusted, &client)?;
    let (listed, fetched) = indexes(repo, &chain, &client)?;
    // Authenticate every head before replacing any, so the hold below is
    // persisted first, as a promotion persists it (`promote_locked`).
    let mut plans = Vec::new();
    for profile in format::PROFILES {
        plans.push((profile, plan_head(repo, &chain, &client, profile)?));
    }
    let taken = plans
        .iter()
        .any(|(_, (pulled, _))| matches!(pulled, Pulled::Taken { .. }));
    let lasting = plans
        .iter()
        .any(|(_, (pulled, _))| matches!(pulled, Pulled::Taken { lasting: true, .. }));
    // The selection another publisher superseded keeps the collection grace
    // its promotion there gave it (`upload.rs` holds on the same rule).
    if taken && (lasting || publisher::retention_hold(repo)?.is_some()) {
        publisher::hold_retention(repo)?;
    }
    let mut heads = Vec::new();
    for (profile, (pulled, write)) in &plans {
        if let Some(value) = write {
            replace(&repo.join("heads").join(profile), value.as_bytes())?;
        }
        heads.push(format!("{profile} {}", pulled.describe()));
    }
    println!(
        "release pull: {} roots ({roots_fetched} fetched), {listed} indexes ({fetched} fetched); {}",
        chain.len(),
        heads.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn at(root: u64, version: u64) -> Head {
        Head {
            root,
            version,
            issued: 0,
            expires: 0,
            profile: "system-x86-64".into(),
            release: "r".into(),
        }
    }

    #[test]
    fn the_bucket_wins_unless_the_local_head_is_ahead_of_it() {
        assert_eq!(choose(None, ("b", &at(1, 1))), Choice::Take);
        assert_eq!(
            choose(Some(("a", &at(1, 3))), ("a", &at(1, 3))),
            Choice::Keep
        );
        // Promoted here, upload pending.
        assert_eq!(
            choose(Some(("a", &at(1, 4))), ("b", &at(1, 3))),
            Choice::Keep
        );
        assert_eq!(
            choose(Some(("a", &at(2, 1))), ("b", &at(1, 9))),
            Choice::Keep
        );
        // Another publisher's promotion, newer or at the same counter.
        assert_eq!(
            choose(Some(("a", &at(1, 3))), ("b", &at(1, 4))),
            Choice::Take
        );
        assert_eq!(
            choose(Some(("a", &at(1, 3))), ("b", &at(1, 3))),
            Choice::Take
        );
        assert_eq!(
            choose(Some(("a", &at(1, 9))), ("b", &at(2, 1))),
            Choice::Take
        );
    }
}
