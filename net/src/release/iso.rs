//! Detached whole-file ISO assertions; firmware does not consume this format.
use super::*;
use crate::s3;

struct Record {
    name: String,
    size: u64,
    digest: String,
    commit: String,
    expires: u64,
}
impl Record {
    fn parse(text: &str, root: &Root, profile: &str, time: u64) -> Result<Self> {
        if text.len() as u64 > SMALL {
            return Err("ISO signature record too large".into());
        }
        root.fresh(time)?;
        format::profile(profile)?;
        let mut lines = format::Lines::new(format::verified(text, &root.release_key)?)?;
        lines.expect("td-release-iso-v1")?;
        if lines.num()? != root.version {
            return Err("ISO signature root epoch mismatch".into());
        }
        let issued = lines.num()?;
        let expires = lines.num()?;
        format::lifetime(issued, expires, time, 30 * format::DAY)?;
        if expires > root.expires {
            return Err("ISO signature outlives its root".into());
        }
        lines.expect(profile)?;
        let commit = lines.next()?.to_string();
        format::hex(&commit, 20)?;
        let name = lines.next()?.to_string();
        filename(&name)?;
        let size = lines.num()?;
        if size == 0 || size > s3::MAX_FILE {
            return Err("ISO size must be between one byte and four GiB".into());
        }
        let digest = lines.next()?.to_string();
        format::hex(&digest, 32)?;
        lines.end()?;
        Ok(Self {
            name,
            size,
            digest,
            commit,
            expires,
        })
    }
    fn image_key(&self) -> String {
        format!("iso/{}/{}", self.digest, self.name)
    }
}

fn filename(name: &str) -> Result<()> {
    if name.len() > 128
        || !name.ends_with(".iso")
        || !name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(
            "ISO filename must be an ASCII basename ending in .iso, at most 128 bytes".into(),
        );
    }
    Ok(())
}
fn image_name(image: &Path) -> Result<&str> {
    let name = image
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("ISO filename is not UTF-8")?;
    filename(name)?;
    Ok(name)
}
fn open_text(path: &Path) -> Result<(File, String)> {
    let (mut file, meta) = io(td_fs::open_real_file(path, "ISO signing metadata"))?;
    if meta.len() > SMALL {
        return Err("ISO signing metadata too large".into());
    }
    let mut bytes = Vec::new();
    io(Read::by_ref(&mut file)
        .take(SMALL + 1)
        .read_to_end(&mut bytes))?;
    if bytes.len() as u64 > SMALL {
        return Err("ISO signing metadata too large".into());
    }
    Ok((
        file,
        String::from_utf8(bytes).map_err(|_| "ISO metadata is not UTF-8")?,
    ))
}

pub(super) fn sign(
    root: &Path,
    key: &Path,
    profile: &str,
    commit: &str,
    image: &Path,
    output: &Path,
) -> Result<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(3600);
    format::profile(profile)?;
    format::hex(commit, 20)?;
    let root = Root::parse(&text(root, SMALL)?, None)?;
    root.fresh(now()?)?;
    let (mut private, meta) = io(td_fs::open_real_file(key, "ISO release signing key"))?;
    if meta.uid() != io(fs::metadata("/proc/self"))?.uid()
        || meta.mode() & 0o077 != 0
        || meta.len() > SMALL
    {
        return Err("ISO signing key must be an owned private regular file within 16 KiB".into());
    }
    let mut key = Vec::new();
    io(Read::by_ref(&mut private)
        .take(SMALL + 1)
        .read_to_end(&mut key))?;
    if key.len() as u64 > SMALL || sig::public_key(&key)?.as_slice() != root.release_key {
        return Err("ISO signing key does not match the public root".into());
    }
    let name = image_name(image)?;
    let (mut file, meta) = io(td_fs::open_real_file(image, "ISO to sign"))?;
    let digest = s3::file_hash_until(&mut file, meta.len(), deadline)?;
    let issued = now()?;
    let expires = issued
        .checked_add(30 * format::DAY)
        .ok_or("ISO expiry overflow")?
        .min(root.expires);
    let body = format!(
        "td-release-iso-v1\n{}\n{issued}\n{expires}\n{profile}\n{commit}\n{name}\n{}\n{digest}\n",
        root.version,
        meta.len()
    );
    let signed = format::signed(&body, &key)?;
    Record::parse(&signed, &root, profile, now()?)?;
    directory(output)?;
    let _lock = lock(&output.join("lock"))?;
    let destination = output.join(format!("{}.sig", hash(signed.as_bytes())));
    publisher::immutable(&destination, signed.as_bytes())?;
    Ok(destination)
}

struct Verified {
    record: Record,
    root: Root,
    root_file: File,
    signature: String,
    signature_file: File,
    image: File,
}
fn verify(
    root_path: &Path,
    profile: &str,
    signature_path: &Path,
    image: &Path,
    deadline: Instant,
) -> Result<Verified> {
    let (root_file, root_text) = open_text(root_path)?;
    let root = Root::parse(&root_text, None)?;
    let (signature_file, signature) = open_text(signature_path)?;
    let record = Record::parse(&signature, &root, profile, now()?)?;
    if image_name(image)? != record.name {
        return Err("ISO filename does not match its signed record".into());
    }
    let (mut file, meta) = io(td_fs::open_real_file(image, "ISO to verify"))?;
    if meta.len() != record.size
        || s3::file_hash_until(&mut file, record.size, deadline)? != record.digest
    {
        return Err("ISO size or SHA-256 does not match its signed record".into());
    }
    Record::parse(&signature, &root, profile, now()?)?;
    Ok(Verified {
        record,
        root,
        root_file,
        signature,
        signature_file,
        image: file,
    })
}
pub(super) fn verify_command(
    root: &Path,
    profile: &str,
    signature: &Path,
    image: &Path,
) -> Result<()> {
    let verified = verify(
        root,
        profile,
        signature,
        image,
        Instant::now() + Duration::from_secs(3600),
    )?;
    println!(
        "{} {} {} {}",
        verified.record.commit, verified.record.digest, verified.record.size, verified.record.name
    );
    Ok(())
}

pub(super) fn upload(words: &[&str]) -> Result<()> {
    let (words, dry) = if words.last() == Some(&"--dry-run") {
        (words.get(..words.len() - 1).ok_or(HELP)?, true)
    } else {
        (words, false)
    };
    let [root, profile, signature, image, endpoint, region, bucket, credentials, credential_profile] =
        words
    else {
        return Err(HELP.into());
    };
    let mut client = s3::Client::new(
        endpoint,
        region,
        bucket,
        Path::new(credentials),
        credential_profile,
    )?;
    let mut verified = verify(
        Path::new(root),
        profile,
        Path::new(signature),
        Path::new(image),
        client.deadline(),
    )?;
    let image_key = verified.record.image_key();
    let signature_digest = hash(verified.signature.as_bytes());
    let signature_key = format!("iso/signatures/{signature_digest}.sig");
    let root_key = format!("roots/{}", verified.root.version);
    println!(
        "ISO S3 {}: {image_key}, {root_key}, {signature_key}; expires {}",
        if dry { "dry run" } else { "upload" },
        verified.record.expires
    );
    if dry {
        return Ok(());
    }
    client.immutable(
        &image_key,
        &mut verified.image,
        verified.record.size,
        &verified.record.digest,
    )?;
    client.immutable(
        &root_key,
        &mut verified.root_file,
        verified.root.text.len() as u64,
        &hash(verified.root.text.as_bytes()),
    )?;
    Record::parse(&verified.signature, &verified.root, profile, now()?)?;
    client.immutable(
        &signature_key,
        &mut verified.signature_file,
        verified.signature.len() as u64,
        &signature_digest,
    )?;
    println!(
        "ISO S3 complete: {} files uploaded, {} reused",
        client.uploaded, client.reused
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Fixture {
        dir: crate::test_temp::TempDir,
        root: PathBuf,
        key: PathBuf,
        image: PathBuf,
        signature: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = crate::test_temp::TempDir::new("iso-signing").unwrap();
            let root = dir.join("root");
            let root_key = dir.join("root.key");
            let key = dir.join("release.key");
            let public = dir.join("release.pub");
            fs::write(&root_key, sig::keygen().unwrap().0).unwrap();
            let (secret, bytes) = sig::keygen().unwrap();
            fs::write(&key, secret).unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&public, sig::to_hex(&bytes)).unwrap();
            publisher::root(
                1,
                now().unwrap() + 365 * format::DAY,
                &root_key,
                &public,
                &root_key,
                &root,
            )
            .unwrap();
            let image = dir.join("td.iso");
            fs::write(&image, b"whole ISO bytes").unwrap();
            let signature = sign(
                &root,
                &key,
                "system-x86-64",
                &"a".repeat(40),
                &image,
                &dir.join("signatures"),
            )
            .unwrap();
            Self {
                dir,
                root,
                key,
                image,
                signature,
            }
        }
        fn verify(&self) -> Result<()> {
            verify_command(&self.root, "system-x86-64", &self.signature, &self.image)
        }
        fn upload(&self, mock: &s3::tests::Mock, dry: bool) -> Result<()> {
            let credentials = self.dir.join("credentials");
            s3::tests::write_credentials(&credentials);
            let mut args = vec![
                self.root.to_str().unwrap(),
                "system-x86-64",
                self.signature.to_str().unwrap(),
                self.image.to_str().unwrap(),
                &mock.endpoint,
                "auto",
                "td-releases",
                credentials.to_str().unwrap(),
                "test",
            ];
            if dry {
                args.push("--dry-run");
            }
            upload(&args)
        }
    }
    #[test]
    fn roundtrip_and_byte_identity() {
        let f = Fixture::new();
        f.verify().unwrap();
        let signed = fs::read(&f.signature).unwrap();
        assert_eq!(
            f.signature.file_name().unwrap().to_str().unwrap(),
            format!("{}.sig", hash(&signed))
        );
        assert!(verify_command(&f.root, "server-x86-64", &f.signature, &f.image).is_err());
        let renamed = f.dir.join("other.iso");
        fs::copy(&f.image, &renamed).unwrap();
        assert!(verify_command(&f.root, "system-x86-64", &f.signature, &renamed).is_err());
        fs::write(&f.image, b"whole ISO byte!").unwrap();
        assert!(f.verify().is_err());
        fs::write(&f.image, b"short").unwrap();
        assert!(f.verify().is_err());
    }
    #[test]
    fn signed_record_bounds_and_tampering() {
        let f = Fixture::new();
        let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
        let signed = text(&f.signature, SMALL).unwrap();
        let time = now().unwrap();
        let record = Record::parse(&signed, &root, "system-x86-64", time).unwrap();
        assert!(Record::parse(&signed, &root, "system-x86-64", record.expires).is_err());
        assert!(Record::parse(
            &signed.replace("td.iso", "xx.iso"),
            &root,
            "system-x86-64",
            time
        )
        .is_err());
        let original: Vec<_> = signed.lines().map(str::to_string).collect();
        for (field, value) in [
            (1, "2".into()),
            (2, (time + 3600).to_string()),
            (3, (time + 31 * format::DAY).to_string()),
            (5, "z".repeat(40)),
            (6, "../td.iso".into()),
            (7, "0".into()),
            (7, (s3::MAX_FILE + 1).to_string()),
            (7, "014".into()),
            (8, "A".repeat(64)),
        ] {
            let mut lines = original.clone();
            lines[field] = value;
            lines.pop();
            let malformed =
                format::signed(&(lines.join("\n") + "\n"), &fs::read(&f.key).unwrap()).unwrap();
            assert!(
                Record::parse(&malformed, &root, "system-x86-64", time).is_err(),
                "field {field}"
            );
        }
    }
    #[test]
    fn key_and_path_refusals() {
        let f = Fixture::new();
        let output = f.dir.join("refused");
        let attempt = || {
            sign(
                &f.root,
                &f.key,
                "system-x86-64",
                &"a".repeat(40),
                &f.image,
                &output,
            )
        };
        fs::set_permissions(&f.key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(attempt().is_err());
        fs::set_permissions(&f.key, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&f.key, sig::keygen().unwrap().0).unwrap();
        assert!(attempt().is_err());
        assert!(!output.exists());
        fs::remove_file(&f.image).unwrap();
        std::os::unix::fs::symlink(&f.signature, &f.image).unwrap();
        assert!(f.verify().is_err());
    }
    #[test]
    fn upload_orders_signature_last_reuses_and_dry_run_is_offline() {
        let f = Fixture::new();
        let mock = s3::tests::Mock::new();
        f.upload(&mock, true).unwrap();
        assert!(mock.store.lock().unwrap().requests.is_empty());
        f.upload(&mock, false).unwrap();
        f.upload(&mock, false).unwrap();
        let store = mock.store.lock().unwrap();
        let puts: Vec<_> = store.requests.iter().filter(|(m, _)| m == "PUT").collect();
        assert_eq!(puts.len(), 3);
        assert!(puts[0].1.ends_with("/td.iso"));
        assert!(puts[1].1.ends_with("roots/1"));
        assert!(puts[2].1.contains("iso/signatures/"));
        drop(store);
        let bad = s3::tests::Mock::new();
        fs::write(&f.image, b"corrupted").unwrap();
        assert!(f.upload(&bad, false).is_err());
        assert!(bad.store.lock().unwrap().requests.is_empty());
    }
    #[test]
    fn failed_image_upload_never_publishes_signature() {
        let f = Fixture::new();
        let mock = s3::tests::Mock::new();
        let digest = hash(&fs::read(&f.image).unwrap());
        mock.store.lock().unwrap().fail = Some((format!("iso/{digest}/td.iso"), 403, 1));
        assert!(f.upload(&mock, false).is_err());
        assert!(mock.store.lock().unwrap().files.is_empty());
    }
}
