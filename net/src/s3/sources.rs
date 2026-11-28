//! `td-net s3 sources`: publish the warmed bytes of every source pin to the
//! source mirror (net/MIRROR.md), keyed by SHA-256.
use super::*;
use crate::feed::{self, SourcePin, SOURCE_MIRROR_PREFIX};
use std::io::Write;

/// The pin rosters a source build warms: `warm sources`, `warm build-root`.
const PIN_SETS: &[&str] = &["source-pins", "build-root-pins"];
const KEY_BYTES: usize = 72;

pub(super) struct Bucket<'a> {
    pub(super) endpoint: &'a str,
    pub(super) region: &'a str,
    pub(super) bucket: &'a str,
    pub(super) credentials: &'a Path,
    pub(super) profile: &'a str,
}

#[derive(Default)]
pub(super) struct Tally {
    pub(super) verified: u64,
    pub(super) uploaded: u64,
    pub(super) reused: u64,
    pub(super) missing: u64,
}

pub(super) fn command(dir: &Path, bucket: &Bucket<'_>, dry: bool) -> Result<()> {
    let root = feed::repo_root();
    let mut tally = Tally::default();
    for verb in PIN_SETS {
        let pins = feed::recipe_pins_result(&root, verb)?;
        upload(&pins, dir, bucket, dry, &mut tally)?;
    }
    outcome(&tally)
}

fn outcome(tally: &Tally) -> Result<()> {
    println!(
        "S3 sources: {} verified, {} uploaded, {} already present, {} missing",
        tally.verified, tally.uploaded, tally.reused, tally.missing
    );
    if tally.missing != 0 {
        return Err("some pins were missing, mismatched or too large: run td-net feed warm sources and td-net feed warm build-root, then rerun".into());
    }
    Ok(())
}

/// The largest keep list `sources-gc` reads: far above any real roster union.
const MAX_KEEP_BYTES: u64 = 8 * 1024 * 1024;

/// The digests a keep list names, one lowercase SHA-256 per line.
fn keep_digests(text: &str) -> Result<std::collections::BTreeSet<&str>> {
    let mut keep = std::collections::BTreeSet::new();
    for line in text.lines() {
        if !feed::is_sha256_hex(line) {
            return Err("a keep list holds one lowercase SHA-256 per line".into());
        }
        keep.insert(line);
    }
    if keep.is_empty() {
        return Err("an empty keep list would delete the whole source mirror".into());
    }
    Ok(keep)
}

/// `sources-gc`: delete every mirrored digest the keep list does not name.
/// Without `delete` it only plans, checking each candidate's metadata.
pub(super) fn collect(keep: &Path, bucket: &Bucket<'_>, delete: bool) -> Result<()> {
    let (mut file, meta) = io(td_fs::open_real_file(keep, "source mirror keep list"))?;
    if meta.len() > MAX_KEEP_BYTES {
        return Err("source mirror keep list too large".into());
    }
    let mut text = String::new();
    io(Read::by_ref(&mut file)
        .take(MAX_KEEP_BYTES + 1)
        .read_to_string(&mut text))?;
    if text.len() as u64 > MAX_KEEP_BYTES {
        return Err("source mirror keep list too large".into());
    }
    let keep = keep_digests(&text)?;
    let client = Client::new(
        bucket.endpoint,
        bucket.region,
        bucket.bucket,
        bucket.credentials,
        bucket.profile,
    )?;
    collect_keep(&keep, &client, delete)
}

/// Delete every mirrored digest `keep` does not name (or, without
/// `delete`, only plan it), refusing a keep set naming nothing mirrored.
pub(crate) fn collect_keep(
    keep: &std::collections::BTreeSet<&str>,
    client: &Client,
    delete: bool,
) -> Result<()> {
    if keep.is_empty() {
        return Err("an empty keep list would delete the whole source mirror".into());
    }
    // Plan the whole pass first: a keep list naming none of what is mirrored
    // belongs to some other bucket or roster, and deletes nothing.
    let remote = client.digest_keys("sources")?;
    let mut kept = 0u64;
    let mut candidates = Vec::new();
    for key in &remote {
        let digest = key
            .strip_prefix(SOURCE_MIRROR_PREFIX)
            .ok_or("unexpected source mirror key")?;
        if keep.contains(digest) {
            kept += 1;
        } else {
            candidates.push((key, digest));
        }
    }
    if kept == 0 && !candidates.is_empty() {
        return Err(
            "the keep list names nothing the source mirror holds; refusing to empty it".into(),
        );
    }
    let mut removed = 0u64;
    for (key, digest) in candidates {
        client.remaining()?;
        if client.prune(key, &["sources"], digest, None, delete)? {
            println!("{} {key}", if delete { "delete" } else { "would-delete" });
            removed += 1;
        }
    }
    println!(
        "S3 sources-gc {}: {kept} kept, {removed} {}",
        if delete { "delete" } else { "dry run" },
        if delete { "deleted" } else { "to delete" }
    );
    Ok(())
}

/// `sources/SHA256`, written into `buffer`.
fn object_key<'b>(sha256: &str, buffer: &'b mut [u8; KEY_BYTES]) -> Option<&'b str> {
    if !feed::is_sha256_hex(sha256) {
        return None;
    }
    let mut cursor = std::io::Cursor::new(buffer.as_mut_slice());
    write!(cursor, "{SOURCE_MIRROR_PREFIX}{sha256}").ok()?;
    let len = usize::try_from(cursor.position()).ok()?;
    std::str::from_utf8(buffer.get(..len)?).ok()
}

/// Upload each pin's cached file under its key. A missing or mismatched file
/// is counted and skipped; a configuration or transport error stops the run,
/// whose rerun reuses what already arrived.
pub(super) fn upload(
    pins: &[SourcePin],
    dir: &Path,
    bucket: &Bucket<'_>,
    dry: bool,
    tally: &mut Tally,
) -> Result<()> {
    let mut buffer = [0u8; KEY_BYTES];
    for pin in pins {
        let Some(key) = object_key(&pin.sha256, &mut buffer) else {
            eprintln!("td-net s3 sources: {} has no SHA-256 key", pin.key);
            tally.missing += 1;
            continue;
        };
        let (mut file, meta) =
            match td_fs::open_real_file(&dir.join(&pin.file), "source mirror file") {
                Ok(opened) => opened,
                Err(error) => {
                    eprintln!("td-net s3 sources: {}: {error}", pin.key);
                    tally.missing += 1;
                    continue;
                }
            };
        if meta.len() > MAX_FILE {
            eprintln!("td-net s3 sources: {} exceeds the S3 upload bound", pin.key);
            tally.missing += 1;
            continue;
        }
        // One client per pin: each upload gets the client's whole deadline.
        let mut client = Client::new(
            bucket.endpoint,
            bucket.region,
            bucket.bucket,
            bucket.credentials,
            bucket.profile,
        )?;
        let digest = file_hash(&mut file, meta.len(), &client)?;
        if digest != pin.sha256 {
            eprintln!(
                "td-net s3 sources: {} is not the pinned bytes of {}",
                pin.file, pin.key
            );
            tally.missing += 1;
            continue;
        }
        tally.verified += 1;
        println!(
            "S3 {}: {key} ({}), {} bytes",
            if dry { "dry run" } else { "upload" },
            pin.file,
            meta.len()
        );
        if !dry {
            client.immutable(key, &mut file, meta.len(), &digest)?;
            tally.uploaded += client.uploaded;
            tally.reused += client.reused;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::s3::tests::{write_credentials, Mock};

    fn pin(key: &str, file: &str, bytes: &[u8]) -> SourcePin {
        SourcePin {
            key: key.into(),
            url: format!("https://upstream.example/{file}"),
            sha256: hash(bytes),
            file: file.into(),
        }
    }

    #[test]
    fn keys_are_the_prefix_and_a_lowercase_digest() {
        let mut buffer = [0u8; KEY_BYTES];
        let sha = hash(b"x");
        let want = format!("sources/{sha}");
        assert_eq!(object_key(&sha, &mut buffer), Some(want.as_str()));
        assert_eq!(object_key(&sha.to_uppercase(), &mut buffer), None);
        assert_eq!(object_key("../heads/system-x86-64", &mut buffer), None);
    }

    #[test]
    fn pins_upload_once_by_digest_and_absent_bytes_are_counted() {
        let dir = crate::test_temp::TempDir::new("s3-sources").unwrap();
        let cred = dir.join("credentials");
        write_credentials(&cred);
        fs::write(dir.join("a.bin"), b"firmware a").unwrap();
        fs::write(dir.join("stale.bin"), b"not what was pinned").unwrap();
        let pins = [
            pin("a", "a.bin", b"firmware a"),
            pin("stale", "stale.bin", b"pinned bytes"),
            pin("absent", "absent.bin", b"never warmed"),
        ];
        let mock = Mock::new();
        let bucket = Bucket {
            endpoint: &mock.endpoint,
            region: "auto",
            bucket: "td-releases",
            credentials: &cred,
            profile: "test",
        };

        let mut tally = Tally::default();
        upload(&pins, &dir, &bucket, true, &mut tally).unwrap();
        assert!(mock.store.lock().unwrap().requests.is_empty());
        assert_eq!((tally.verified, tally.missing), (1, 2));
        assert!(outcome(&tally).is_err());

        let mut tally = Tally::default();
        upload(&pins, &dir, &bucket, false, &mut tally).unwrap();
        let key = format!("sources/{}", hash(b"firmware a"));
        {
            let store = mock.store.lock().unwrap();
            assert_eq!(store.files.len(), 1);
            assert_eq!(store.files[&key].bytes, b"firmware a");
            assert_eq!(store.files[&key].headers["cache-control"], IMMUTABLE);
        }
        assert_eq!((tally.uploaded, tally.reused, tally.missing), (1, 0, 2));

        let mut tally = Tally::default();
        upload(&pins[..1], &dir, &bucket, false, &mut tally).unwrap();
        assert_eq!((tally.uploaded, tally.reused), (0, 1));
        assert!(outcome(&tally).is_ok());

        // A pin file must be the file itself, never a link to one.
        let link = dir.join("link.bin");
        std::os::unix::fs::symlink(dir.join("a.bin"), &link).unwrap();
        let mut tally = Tally::default();
        let linked = [pin("linked", "link.bin", b"firmware a")];
        upload(&linked, &dir, &bucket, true, &mut tally).unwrap();
        assert_eq!((tally.verified, tally.missing), (0, 1));
    }

    #[test]
    fn collection_keeps_only_listed_digests_and_plans_first() {
        let dir = crate::test_temp::TempDir::new("s3-sources-gc").unwrap();
        let cred = dir.join("credentials");
        write_credentials(&cred);
        let names = ["old.bin", "kept.bin", "older.bin"];
        for name in names {
            fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        let pins: Vec<_> = names
            .iter()
            .map(|name| pin(name, name, name.as_bytes()))
            .collect();
        let mock = Mock::new();
        let bucket = Bucket {
            endpoint: &mock.endpoint,
            region: "auto",
            bucket: "td-releases",
            credentials: &cred,
            profile: "test",
        };
        upload(&pins, &dir, &bucket, false, &mut Tally::default()).unwrap();
        let release = format!("releases/{}", hash(b"index"));
        mock.store.lock().unwrap().files.insert(
            release.clone(),
            crate::s3::tests::Stored {
                bytes: b"index".to_vec(),
                headers: BTreeMap::new(),
            },
        );
        let keep = dir.join("keep");
        fs::write(&keep, format!("{}\n", hash(b"kept.bin"))).unwrap();

        collect(&keep, &bucket, false).unwrap();
        assert_eq!(mock.store.lock().unwrap().files.len(), 4);

        // A list naming nothing mirrored is refused before any deletion.
        let foreign = dir.join("foreign");
        fs::write(&foreign, format!("{}\n", hash(b"elsewhere"))).unwrap();
        assert!(collect(&foreign, &bucket, true).is_err());
        assert_eq!(mock.store.lock().unwrap().files.len(), 4);

        // An object whose upload metadata does not name its key's digest
        // is not deleted, and stops the pass.
        mock.store
            .lock()
            .unwrap()
            .files
            .get_mut(&format!("sources/{}", hash(b"old.bin")))
            .unwrap()
            .headers
            .insert("x-amz-meta-sha256".into(), hash(b"other"));
        assert!(collect(&keep, &bucket, true).is_err());
        assert!(mock
            .store
            .lock()
            .unwrap()
            .files
            .contains_key(&format!("sources/{}", hash(b"old.bin"))));
        mock.store
            .lock()
            .unwrap()
            .files
            .get_mut(&format!("sources/{}", hash(b"old.bin")))
            .unwrap()
            .headers
            .insert("x-amz-meta-sha256".into(), hash(b"old.bin"));

        collect(&keep, &bucket, true).unwrap();
        let store = mock.store.lock().unwrap();
        let left: Vec<_> = store.files.keys().cloned().collect();
        assert_eq!(left, [release, format!("sources/{}", hash(b"kept.bin"))]);
    }

    #[test]
    fn a_keep_list_must_be_nonempty_digests() {
        assert!(keep_digests("").is_err());
        assert!(keep_digests("not-a-digest\n").is_err());
        assert!(keep_digests(&hash(b"x").to_uppercase()).is_err());
        let one = hash(b"x");
        assert_eq!(keep_digests(&format!("{one}\n{one}\n")).unwrap().len(), 1);
    }
}
