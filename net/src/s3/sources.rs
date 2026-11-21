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
}
