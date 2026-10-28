//! Offline replacement of an existing unencrypted installation. No formatter,
//! partition-table writer or recursive removal belongs on this path.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::installation_plan::{Mode, Plan, Settings, Storage};
use crate::{invalid, paths, protocol, Workspace};

fn command(root: &Path, program: &str, args: &[&std::ffi::OsStr]) -> io::Result<Vec<u8>> {
    let output = Command::new(root.join("bin").join(program))
        .env_clear()
        .current_dir("/")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(invalid(format!(
            "reinstall: {program} failed ({})",
            output.status
        )));
    }
    Ok(output.stdout)
}

fn directory(path: &Path) -> io::Result<()> {
    owned_directory(path, 0)
}

fn owned_directory(path: &Path, owner: u32) -> io::Result<()> {
    let metadata = paths::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
        return Err(invalid(format!(
            "{} must be a real root-owned directory",
            path.display()
        )));
    }
    Ok(())
}

fn setting(volume: &Path, name: &str, limit: u64) -> io::Result<String> {
    let path = volume
        .join(protocol::VOLUME_SUBVOL)
        .join("lib/td")
        .join(name);
    let (file, metadata) = crate::td_fs::open_real_file(&path, "saved installation setting")?;
    if metadata.uid() != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o644
        || metadata.len() == 0
        || metadata.len() > limit
    {
        return Err(invalid(format!("invalid saved {name}")));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    let value =
        std::str::from_utf8(&bytes).map_err(|_| invalid(format!("invalid saved {name}")))?;
    value
        .strip_suffix('\n')
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("saved {name} lacks its final newline")))
}

fn settings(volume: &Path) -> io::Result<Settings> {
    for relative in [
        "",
        "@var",
        "@var/lib",
        "@var/lib/td",
        "@var/home",
        "td",
        "td/boot",
        "td/deployments",
    ] {
        directory(&volume.join(relative))?;
    }
    let settings = Settings::new(
        &setting(volume, "username", 33)?,
        &setting(volume, "hostname", 64)?,
        "us",
        &setting(volume, "timezone", 65)?,
    )
    .map_err(invalid)?;
    let home = volume.join("@var/home").join(settings.username());
    let metadata = paths::symlink_metadata(&home)?;
    if !metadata.is_dir() || metadata.uid() != 1000 {
        return Err(invalid(
            "the saved account has no real UID-1000 home directory".into(),
        ));
    }
    Ok(settings)
}

struct Mounted<'a> {
    root: &'a Path,
    at: &'a Path,
    mounted: bool,
}

impl Mounted<'_> {
    fn close(&mut self) -> io::Result<()> {
        if self.mounted {
            command(self.root, "umount", &[self.at.as_os_str()])?;
            self.mounted = false;
        }
        Ok(())
    }
}

impl Drop for Mounted<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            let _ = writeln!(io::stderr(), "reinstall: unmount failed: {error}");
        }
    }
}

fn with_volume<T>(
    claim: &mut File,
    root: &Path,
    workspace: &Workspace,
    write: bool,
    action: impl FnOnce(&Path, &Path, [u8; 16]) -> io::Result<T>,
) -> io::Result<T> {
    let sector = crate::logical_sector_size(claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::volume_region(claim, sector, capacity / sector)?;
    let header = crate::read_at(claim, offset, 6)?;
    if header == b"LUKS\xba\xbe" {
        return Err(invalid(
            "encrypted reinstall is not supported yet; the disk was not changed".into(),
        ));
    }
    let bytes = crate::read_at(claim, offset + 65536, 4096)?;
    if bytes.get(64..72) != Some(b"_BHRfS_M") {
        return Err(invalid("the existing td volume is not Btrfs".into()));
    }
    let uuid: [u8; 16] = bytes
        .get(32..48)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("missing existing filesystem UUID".into()))?;
    let device = crate::loop_device::attach(claim, offset, length, sector)?;
    command(
        root,
        "mount",
        &[
            "-t".as_ref(),
            "btrfs".as_ref(),
            "-o".as_ref(),
            if write {
                "rw,nodev,nosuid,noexec,subvolid=5".as_ref()
            } else {
                "ro,rescue=nologreplay,nodev,nosuid,noexec,subvolid=5".as_ref()
            },
            device.path().as_os_str(),
            workspace.mountpoint.as_os_str(),
        ],
    )?;
    let mut mounted = Mounted {
        root,
        at: &workspace.mountpoint,
        mounted: true,
    };
    // The kernel must have mounted the UUID read above as a single-device td
    // filesystem. A backup superblock or a different device cannot substitute.
    let fs = Path::new("/sys/fs/btrfs").join(crate::canonical_uuid(&uuid));
    let checked = (|| {
        if paths::read_to_string(&fs.join("label"))?.trim_end() != protocol::VOLUME_LABEL {
            return Err(invalid(
                "the mounted filesystem is not labelled td-system".into(),
            ));
        }
        let devices = paths::read_dir_bounded(&fs.join("devices"), 2)?;
        if devices.len() != 1
            || devices.first().and_then(|p| p.file_name()) != device.path().file_name()
        {
            return Err(invalid(
                "reinstall requires the claimed single-device filesystem".into(),
            ));
        }
        action(&workspace.mountpoint, device.path(), uuid)
    })();
    let unmounted = mounted.close();
    drop(mounted);
    let released = device.release();
    let result = checked?;
    unmounted?;
    released?;
    Ok(result)
}

pub(crate) fn inspect(
    claim: &mut File,
    root: &Path,
    workspace: &Workspace,
) -> io::Result<([u8; 16], Settings)> {
    with_volume(claim, root, workspace, false, |volume, _, uuid| {
        Ok((uuid, settings(volume)?))
    })
}

fn approved_manifest(path: &Path, expected: &[u8; 32]) -> io::Result<()> {
    let mut file = paths::open_read(path)?;
    let length = file.metadata()?.len();
    if length > protocol::MAX_MANIFEST_BYTES
        || crate::digest_range(&mut file, 0, length)? != *expected
    {
        return Err(invalid(
            "staged reinstall manifest differs from the approved review".into(),
        ));
    }
    Ok(())
}

fn copy(source: &Path, destination: &Path, limit: u64) -> io::Result<()> {
    let (input, metadata) = crate::td_fs::open_real_file(source, "reinstall source")?;
    if metadata.len() == 0 || metadata.len() > limit {
        return Err(invalid("reinstall input exceeds its bound".into()));
    }
    let mut output = paths::create_new_with_mode(destination, 0o600)?;
    if io::copy(&mut input.take(metadata.len() + 1), &mut output)? != metadata.len() {
        return Err(invalid("reinstall input changed size".into()));
    }
    output.sync_all()
}

const STAGED_FILES: &[&str] = &["manifest", "bzImage", "initramfs.cpio", "root.erofs"];

/// Where, under the volume root, the approved deployment is copied before
/// td-boot publishes it: root-private state on the volume being replaced,
/// so the bytes published are the bytes the review approved whatever the
/// medium does meanwhile.
const STAGE_PARENT: &str = "@var/lib/td-install";

/// The stage's parent on the writable volume, made if absent. Every
/// directory above it is checked first, under this mount, so a link in the
/// volume being replaced cannot send root's writes off it. `owner` is root
/// but in tests.
fn stage_parent(volume: &Path, owner: u32) -> io::Result<std::path::PathBuf> {
    for ancestor in ["", "@var", "@var/lib"] {
        owned_directory(&volume.join(ancestor), owner)?;
    }
    let state = volume.join(STAGE_PARENT);
    match paths::symlink_metadata(&state) {
        Ok(_) => owned_directory(&state, owner)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            paths::create_dir_with_mode(&state, 0o700)?;
        }
        Err(error) => return Err(error),
    }
    Ok(state)
}

// The caller validates root ownership first. Inspect every entry before
// removing anything, and never follow links or recurse into directories.
fn clear_stage(path: &Path) -> io::Result<()> {
    if !paths::symlink_metadata(path)?.is_dir() {
        return Err(invalid("reinstall stage is not a real directory".into()));
    }
    let entries = paths::read_dir_bounded(path, STAGED_FILES.len() + 1)?;
    for entry in &entries {
        if !entry
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| STAGED_FILES.contains(&n))
            || !paths::symlink_metadata(entry)?.is_file()
        {
            return Err(invalid(
                "unexpected entry in interrupted reinstall stage".into(),
            ));
        }
    }
    for entry in entries {
        paths::remove_file_if_present(&entry)?;
    }
    paths::remove_dir_if_present(path)
}

pub(crate) fn execute(
    execution: &crate::LiveExecution,
    plan: &Plan,
    mut claim: File,
    workspace: &Workspace,
    progress: &mut dyn FnMut(crate::installation_protocol::Phase),
) -> io::Result<()> {
    use crate::installation_protocol::Phase;
    if plan.mode() != Mode::Preserve || plan.storage() != Storage::Unencrypted {
        return Err(invalid(
            "reinstall requires an unencrypted preserve plan".into(),
        ));
    }
    // Recheck before the first writable mount. No installer-selected account
    // setting is applied, even when a later retry changes the medium.
    let observed = inspect(&mut claim, &execution.root, workspace)?;
    if observed != (*plan.volume_uuid(), plan.settings().clone()) {
        return Err(invalid(
            "the existing installation changed after review".into(),
        ));
    }
    // Reject an undersized or damaged ESP before staging or publishing a
    // replacement. Its geometry is read from the existing GPT.
    let sector = crate::logical_sector_size(&claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::esp_region(&mut claim, sector, capacity / sector)?;
    let esp = td_fat::Esp::open(claim.try_clone()?, offset, length)
        .map_err(|e| invalid(e.to_string()))?;
    let (kernel, _) = crate::deployment_bytes(&execution.source)?;
    let selector = crate::selector_parts(
        &execution.root.join(protocol::SELECTOR_TEMPLATE_PATH),
        &crate::VolumeUuid::parse(&crate::canonical_uuid(plan.volume_uuid()))?,
    )?
    .total()
    .ok_or_else(|| invalid("selector size overflow".into()))?;
    esp.fits(kernel, selector, false)
        .map_err(|e| invalid(e.to_string()))?;
    drop(esp);
    crate::validate_source(&execution.td_boot, &execution.source).and_then(|id| {
        if plan.matches_deployment_id(&id) {
            Ok(())
        } else {
            Err(invalid("the source changed after review".into()))
        }
    })?;
    let expected_kernel = execution.kernel_digest(plan.deployment())?;
    crate::stage_kernel(&execution.kernel().map_err(|(_, e)| e)?, &workspace.kernel)?;
    let mut staged_kernel = paths::open_read(&workspace.kernel)?;
    let staged_length = staged_kernel.metadata()?.len();
    if crate::digest_range(&mut staged_kernel, 0, staged_length)? != expected_kernel {
        return Err(invalid(
            "reinstall kernel differs from the approved manifest".into(),
        ));
    }
    with_volume(
        &mut claim,
        &execution.root,
        workspace,
        true,
        |volume, device, uuid| {
            progress(Phase::PublishingDeployment);
            crate::prepare_selector(
                &execution.root.join(protocol::SELECTOR_TEMPLATE_PATH),
                &crate::VolumeUuid::parse(&crate::canonical_uuid(&uuid))?,
                &workspace.selector,
            )?;
            let state = stage_parent(volume, 0)?;
            let stage = state.join("reinstall");
            match paths::symlink_metadata(&stage) {
                Ok(_) => {
                    directory(&stage)?;
                    clear_stage(&stage)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            paths::create_dir_with_mode(&stage, 0o700)?;
            let staged: io::Result<()> = (|| {
                for (name, limit) in [
                    ("manifest", protocol::MAX_MANIFEST_BYTES),
                    ("bzImage", 256 * 1024 * 1024),
                    ("initramfs.cpio", 512 * 1024 * 1024),
                    ("root.erofs", 128 * 1024 * 1024 * 1024),
                ] {
                    copy(
                        &crate::source_payload(&execution.source, name)?,
                        &stage.join(name),
                        limit,
                    )?;
                }
                // td-boot checks every payload against this manifest.
                approved_manifest(&stage.join("manifest"), plan.deployment())?;
                let publish = workspace.dir.join("publish");
                paths::create_dir_with_mode(&publish, 0o700)?;
                let installed = command(
                    &execution.root,
                    "td-boot",
                    &[
                        "reinstall".as_ref(),
                        device.as_os_str(),
                        publish.as_os_str(),
                        stage.as_os_str(),
                    ],
                );
                // Never recursively remove a mountpoint, including on failure.
                let removed = paths::remove_dir_if_present(&publish);
                installed?;
                removed?;
                paths::open_read(&volume.join("td"))?.sync_all()?;
                Ok(())
            })();
            if let Err(error) = clear_stage(&stage) {
                let _ = writeln!(io::stderr(), "reinstall: staging cleanup failed: {error}");
            }
            staged?;
            progress(Phase::VerifyingBoot);
            Ok(())
        },
    )?;
    // The filesystem is synced and unmounted before replacing firmware files.
    let sector = crate::logical_sector_size(&claim)?;
    let capacity = claim.seek(SeekFrom::End(0))?;
    let (offset, length) = crate::esp_region(&mut claim, sector, capacity / sector)?;
    let mut esp = td_fat::Esp::open(claim, offset, length).map_err(|e| invalid(e.to_string()))?;
    let mut kernel = paths::open_read(&workspace.kernel)?;
    let mut selector = paths::open_read(&workspace.selector)?;
    esp.write_direct(
        td_fat::Input {
            len: kernel.metadata()?.len(),
            reader: &mut kernel,
        },
        td_fat::Input {
            len: selector.metadata()?.len(),
            reader: &mut selector,
        },
    )
    .map_err(|e| invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn changed_media_manifest_cannot_be_published_as_the_approved_deployment() {
        let temp = ::td_fs::TempDir::new("reinstall-manifest").unwrap();
        let source = temp.path().join("source");
        let staged = temp.path().join("manifest");
        let reviewed = b"approved manifest with original root digest";
        let mut hash = crate::sha256::Sha256::new();
        hash.update(reviewed);
        let expected = hash.finalize();
        fs::write(&source, reviewed).unwrap();
        copy(&source, &staged, protocol::MAX_MANIFEST_BYTES).unwrap();
        approved_manifest(&staged, &expected).unwrap();
        fs::remove_file(&staged).unwrap();
        fs::write(
            &source,
            b"replacement manifest with unchanged kernel and substituted root",
        )
        .unwrap();
        copy(&source, &staged, protocol::MAX_MANIFEST_BYTES).unwrap();
        assert!(approved_manifest(&staged, &expected).is_err());
    }

    #[test]
    fn iso_payload_names_are_resolved_before_staging() {
        let temp = ::td_fs::TempDir::new("reinstall-iso-names").unwrap();
        let source = temp.path().join("source");
        let stage = temp.path().join("stage");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&stage).unwrap();
        for (iso, name) in protocol::MEDIA_DEPLOYMENT_FILES {
            fs::write(source.join(iso.to_ascii_lowercase()), name.as_bytes()).unwrap();
            copy(
                &crate::source_payload(&source, name).unwrap(),
                &stage.join(name),
                1024,
            )
            .unwrap();
            assert_eq!(fs::read(stage.join(name)).unwrap(), name.as_bytes());
        }
    }

    #[test]
    fn workspace_never_walks_a_publish_mountpoint() {
        let temp = ::td_fs::TempDir::new("reinstall-workspace").unwrap();
        for held in [false, true] {
            let workspace =
                Workspace::create(temp.path(), &[if held { 2 } else { 1 }; 32]).unwrap();
            let publish = workspace.dir.join("publish");
            fs::create_dir(&publish).unwrap();
            if held {
                fs::write(publish.join("personal"), b"keep").unwrap();
            }
            assert_eq!(workspace.remove().is_err(), held);
            if held {
                assert_eq!(fs::read(publish.join("personal")).unwrap(), b"keep");
            } else {
                assert!(!publish.exists());
            }
        }
    }

    #[test]
    fn the_stage_parent_is_made_only_beneath_checked_directories() {
        use std::os::unix::fs::PermissionsExt;
        let temp = ::td_fs::TempDir::new("reinstall-stage-parent").unwrap();
        let owner = fs::metadata(temp.path()).unwrap().uid();
        let volume = temp.path().join("volume");
        let elsewhere = temp.path().join("elsewhere");
        for directory in [&volume, &volume.join("@var"), &elsewhere] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A link where `@var/lib` belongs: refused, and nothing made there.
        symlink(&elsewhere, volume.join("@var/lib")).unwrap();
        assert!(stage_parent(&volume, owner).is_err());
        assert!(!elsewhere.join("td-install").exists());
        fs::remove_file(volume.join("@var/lib")).unwrap();
        fs::create_dir(volume.join("@var/lib")).unwrap();
        fs::set_permissions(volume.join("@var/lib"), fs::Permissions::from_mode(0o755)).unwrap();
        // A parent others may write is refused too.
        fs::set_permissions(volume.join("@var"), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(stage_parent(&volume, owner).is_err());
        fs::set_permissions(volume.join("@var"), fs::Permissions::from_mode(0o755)).unwrap();
        // Another owner's tree is refused; ours is made at 0700.
        assert!(stage_parent(&volume, owner + 1).is_err());
        let state = stage_parent(&volume, owner).unwrap();
        assert_eq!(state, volume.join(STAGE_PARENT));
        assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o700);
        assert_eq!(stage_parent(&volume, owner).unwrap(), state);
    }

    #[test]
    fn interrupted_stage_is_reclaimed_without_touching_personal_files() {
        let temp = ::td_fs::TempDir::new("reinstall-stage").unwrap();
        let home = temp.path().join("home");
        fs::write(&home, b"personal bytes").unwrap();
        let stage = temp.path().join("reinstall");
        for _ in 0..2 {
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("manifest"), b"partial manifest").unwrap();
            fs::write(stage.join("root.erofs"), b"partial payload").unwrap();
            clear_stage(&stage).unwrap();
            assert!(!stage.exists());
            assert_eq!(fs::read(&home).unwrap(), b"personal bytes");
        }
    }

    #[test]
    fn unexpected_stage_contents_and_links_refuse_before_removing_any_file() {
        let temp = ::td_fs::TempDir::new("reinstall-stage-refusal").unwrap();
        let stage = temp.path().join("reinstall");
        fs::create_dir(&stage).unwrap();
        let manifest = stage.join("manifest");
        fs::write(&manifest, b"keep until validated").unwrap();
        let extra = stage.join("unexpected");
        fs::write(&extra, b"keep").unwrap();
        assert!(clear_stage(&stage).is_err());
        assert!(manifest.exists());
        fs::remove_file(&extra).unwrap();
        symlink(&manifest, stage.join("root.erofs")).unwrap();
        assert!(clear_stage(&stage).is_err());
        assert!(manifest.exists());
        let alias = temp.path().join("alias");
        symlink(&stage, &alias).unwrap();
        assert!(clear_stage(&alias).is_err());
        assert_eq!(fs::read(&manifest).unwrap(), b"keep until validated");
    }
}
