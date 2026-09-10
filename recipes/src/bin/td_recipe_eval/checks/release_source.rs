//! The committed source accompanying a published VM, outside the target graph.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub(crate) const VOLUME_DIRECTORY: &str = "td/source";
pub(crate) const BUNDLE_NAME: &str = "repository.bundle";
pub(crate) const REVISION_NAME: &str = "revision";

pub(crate) fn payload_bytes(source: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for name in [BUNDLE_NAME, REVISION_NAME] {
        let path = source.join(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("inspect release source {}: {e}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!(
                "release source {} is not a regular file",
                path.display()
            ));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or("release source size overflow")?;
    }
    Ok(total)
}

pub(crate) fn copy_to_volume(source: &Path, volume: &Path) -> Result<(), String> {
    payload_bytes(source)?;
    let destination = volume.join(VOLUME_DIRECTORY);
    fs::create_dir(&destination).map_err(|e| {
        format!(
            "create volume source directory {}: {e}",
            destination.display()
        )
    })?;
    for name in [BUNDLE_NAME, REVISION_NAME] {
        let from = source.join(name);
        let to = destination.join(name);
        fs::copy(&from, &to).map_err(|e| {
            format!(
                "copy release source {} to {}: {e}",
                from.display(),
                to.display()
            )
        })?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("set release source permissions {}: {e}", to.display()))?;
    }
    // mkfs copies these modes verbatim, including a restrictive host umask.
    for directory in [volume.to_path_buf(), volume.join("td"), destination] {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).map_err(|e| {
            format!(
                "set release directory permissions {}: {e}",
                directory.display()
            )
        })?;
    }
    Ok(())
}

pub(crate) struct ReleaseSource {
    revision: String,
}

fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["--no-replace-objects", "-C"])
        .arg(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .stdin(Stdio::null());
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    command
}

fn output(command: &mut Command, action: &str) -> Result<Output, String> {
    let output = command.output().map_err(|e| format!("{action}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{action} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn query(root: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = output(git(root).args(args), "inspect release source")?.stdout;
    String::from_utf8(bytes)
        .map(|text| text.trim_end().to_string())
        .map_err(|_| "release source metadata is not UTF-8".into())
}

impl ReleaseSource {
    /// A published release must be recoverable from its accompanying commit.
    pub(crate) fn inspect(root: &Path) -> Result<Self, String> {
        let top = query(root, &["rev-parse", "--show-toplevel"])?;
        let top = Path::new(&top)
            .canonicalize()
            .map_err(|e| format!("resolve Git checkout root {top}: {e}"))?;
        let canonical = root
            .canonicalize()
            .map_err(|e| format!("resolve release source directory {}: {e}", root.display()))?;
        if top != canonical {
            return Err("publish a release from the Git checkout root".into());
        }
        if query(root, &["rev-parse", "--is-shallow-repository"])? != "false" {
            return Err(
                "release source requires complete Git history; unshallow the checkout".into(),
            );
        }
        if !query(root, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
            return Err("release source has changes or untracked files; commit or remove them before publishing".into());
        }
        let revision = query(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        if !matches!(revision.len(), 40 | 64)
            || !revision
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err("release source has an invalid Git commit ID".into());
        }
        Ok(Self { revision })
    }

    pub(crate) fn revision(&self) -> &str {
        &self.revision
    }

    pub(crate) fn verify_checkout(&self, root: &Path) -> Result<(), String> {
        if Self::inspect(root)?.revision != self.revision {
            return Err(
                "release source commit changed during publication; retry from a stable checkout"
                    .into(),
            );
        }
        Ok(())
    }

    /// Only HEAD's reachable history is exported, never local refs or configuration.
    pub(crate) fn stage(&self, root: &Path, directory: &Path) -> Result<(), String> {
        self.verify_checkout(root)?;
        fs::create_dir(directory)
            .map_err(|e| format!("create release source {}: {e}", directory.display()))?;
        let bundle = directory.join(BUNDLE_NAME);
        output(
            git(root)
                .args(["bundle", "create"])
                .arg(&bundle)
                .arg("HEAD"),
            "export release source bundle",
        )?;
        let heads = output(
            git(root).args(["bundle", "list-heads"]).arg(&bundle),
            "inspect exported release source",
        )?;
        if heads.stdout != format!("{} HEAD\n", self.revision).as_bytes() {
            return Err("exported release source does not name the selected commit".into());
        }
        output(
            git(root).args(["bundle", "verify"]).arg(&bundle),
            "verify exported release source",
        )?;
        self.verify_checkout(root)?;
        fs::write(
            directory.join(REVISION_NAME),
            format!("{}\n", self.revision),
        )
        .map_err(|e| format!("write release source revision {}: {e}", directory.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn directory() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "td-release-source-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn new() -> Self {
            Self::with_object_format("sha1")
        }

        fn with_object_format(format: &str) -> Self {
            let fixture = Self::directory();
            output(
                git(&fixture.0).args(["init", "-b", "main", "--object-format", format]),
                "init fixture",
            )
            .unwrap();
            fixture.commit("initial");
            fixture
        }

        fn commit(&self, contents: &str) {
            fs::write(self.0.join("tracked"), contents).unwrap();
            output(git(&self.0).args(["add", "tracked"]), "stage fixture").unwrap();
            output(
                git(&self.0).args([
                    "-c",
                    "user.name=release test",
                    "-c",
                    "user.email=release@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "-m",
                    contents,
                ]),
                "commit fixture",
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn source_volume_is_readable_and_contains_only_the_export() {
        let scratch = Fixture::directory();
        let source = scratch.0.join("source");
        let volume = scratch.0.join("volume");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(volume.join("td")).unwrap();
        for name in [BUNDLE_NAME, REVISION_NAME, "private"] {
            fs::write(source.join(name), name).unwrap();
            fs::set_permissions(source.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::set_permissions(volume.join("td"), fs::Permissions::from_mode(0o700)).unwrap();
        copy_to_volume(&source, &volume).unwrap();
        let installed = volume.join(VOLUME_DIRECTORY);
        assert_eq!(fs::read_dir(&installed).unwrap().count(), 2);
        for name in [BUNDLE_NAME, REVISION_NAME] {
            let path = installed.join(name);
            assert_eq!(fs::read_to_string(&path).unwrap(), name);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        for path in [&volume, &volume.join("td"), &installed] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        assert_eq!(
            payload_bytes(&source).unwrap(),
            (BUNDLE_NAME.len() + REVISION_NAME.len()) as u64
        );
        fs::remove_file(source.join(BUNDLE_NAME)).unwrap();
        std::os::unix::fs::symlink(REVISION_NAME, source.join(BUNDLE_NAME)).unwrap();
        assert!(payload_bytes(&source).is_err());
    }

    #[test]
    #[ignore = "requires host Git; run release_source tests with --ignored"]
    fn exported_history_clones_offline_without_private_refs_or_configuration() {
        let fixture = Fixture::new();
        output(
            git(&fixture.0).args(["branch", "private-topic"]),
            "private ref",
        )
        .unwrap();
        output(
            git(&fixture.0).args([
                "config",
                "remote.origin.url",
                "ssh://private.invalid/secret",
            ]),
            "private config",
        )
        .unwrap();
        fixture.commit("release");
        let release = ReleaseSource::inspect(&fixture.0).unwrap();
        let scratch = fixture.0.join(".git/export");
        release.stage(&fixture.0, &scratch).unwrap();
        let clone = fixture.0.join(".git/clone");
        output(
            git(&fixture.0)
                .args(["clone", "--no-local", "--"])
                .arg(scratch.join(BUNDLE_NAME))
                .arg(&clone),
            "clone bundle",
        )
        .unwrap();
        assert_eq!(
            query(&clone, &["rev-parse", "HEAD"]).unwrap(),
            release.revision()
        );
        assert_eq!(
            query(&clone, &["rev-list", "--count", "HEAD"]).unwrap(),
            "2"
        );
        assert_eq!(
            fs::read_to_string(clone.join("tracked")).unwrap(),
            "release"
        );
        assert!(!query(&clone, &["for-each-ref"])
            .unwrap()
            .contains("private-topic"));
        assert!(!query(&clone, &["config", "--local", "--list"])
            .unwrap()
            .contains("private.invalid"));
        assert_eq!(
            fs::read_to_string(scratch.join(REVISION_NAME)).unwrap(),
            format!("{}\n", release.revision())
        );
    }

    #[test]
    #[ignore = "requires host Git; run release_source tests with --ignored"]
    fn sha256_history_clones_offline() {
        let fixture = Fixture::with_object_format("sha256");
        let release = ReleaseSource::inspect(&fixture.0).unwrap();
        assert_eq!(release.revision().len(), 64);
        let scratch = fixture.0.join(".git/export");
        release.stage(&fixture.0, &scratch).unwrap();
        let clone = fixture.0.join(".git/clone");
        output(
            git(&fixture.0)
                .arg("clone")
                .arg(scratch.join(BUNDLE_NAME))
                .arg(&clone),
            "clone SHA-256 bundle",
        )
        .unwrap();
        assert_eq!(
            query(&clone, &["rev-parse", "HEAD"]).unwrap(),
            release.revision()
        );
    }

    #[test]
    #[ignore = "requires host Git; run release_source tests with --ignored"]
    fn publication_refuses_dirty_untracked_and_moved_source() {
        let fixture = Fixture::new();
        let release = ReleaseSource::inspect(&fixture.0).unwrap();
        fs::write(fixture.0.join("untracked"), "not in the release").unwrap();
        assert!(release.verify_checkout(&fixture.0).is_err());
        fs::remove_file(fixture.0.join("untracked")).unwrap();
        fs::write(fixture.0.join("tracked"), "dirty").unwrap();
        assert!(release.verify_checkout(&fixture.0).is_err());
        fixture.commit("next release");
        assert!(release.verify_checkout(&fixture.0).is_err());
        assert!(release
            .stage(&fixture.0, &fixture.0.join(".git/export"))
            .is_err());
    }
}
