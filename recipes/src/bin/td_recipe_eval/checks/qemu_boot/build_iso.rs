//! Write a bootable td installation medium: the signed system deployment on a
//! hybrid ISO whose firmware entry boots the live selector.
//!
//! The ISO is the live profile of td-install/MEDIA.md "Live boot": its ESP holds
//! the kernel and a selector initramfs carrying this run's public key and the
//! live marker, and its root holds the deployment td-boot authenticates and
//! boots from the medium. Like `bundle`, it signs with a key made for this run
//! and discarded; the ISO carries only the public half.
//!
//! The payloads are streamed from the verified store deployment; only the
//! manifest is copied, so its signature can be written beside it.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use super::{
    build_system, create_scratch_dir, media, provision_live_selector, RunTrust, Scratch,
    VerifiedSelector,
};
use crate::check_runner::RecipeCheckRunner;
use td_recipe::td_boot_protocol::{
    MANIFEST_NAME, MANIFEST_SIG_NAME, MEDIA_DEPLOYMENT_FILES, MEDIA_VOLUME_ID,
};

/// Where the ISO goes when the operator does not say. Repo-relative and
/// gitignored, like a bundle.
pub(crate) const DEFAULT_OUT: &str = "dist/td-install-x86-64.iso";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildIsoOptions {
    pub(crate) out: PathBuf,
    /// Replace an existing td installation ISO at `out`.
    pub(crate) force: bool,
}

pub(crate) fn usage() -> String {
    format!(
        "usage: build-iso [--out FILE] [--force]\n       \
         --out FILE  where to write the ISO (default {DEFAULT_OUT})\n       \
         --force     replace a td installation ISO already at FILE; any other\n       \
         \x20           file is refused"
    )
}

pub(crate) fn parse_args(args: &[String]) -> Result<BuildIsoOptions, String> {
    let mut out = None;
    let mut force = false;
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--force" => force = true,
            "--out" => {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("--out needs a file\n{}", usage()))?;
                if out.replace(PathBuf::from(value)).is_some() {
                    return Err(format!("--out given twice\n{}", usage()));
                }
            }
            other => return Err(format!("unknown argument '{other}'\n{}", usage())),
        }
    }
    Ok(BuildIsoOptions {
        out: out.unwrap_or_else(|| PathBuf::from(DEFAULT_OUT)),
        force,
    })
}

/// Whether `path` holds a td installation medium, judged by its primary
/// volume descriptor as td-boot judges one. `--force` removes nothing else.
fn is_td_iso(path: &Path) -> Result<bool, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    let mut descriptor = [0u8; 72];
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(16 * 2048))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    if file.read_exact(&mut descriptor).is_err() {
        return Ok(false);
    }
    let mut label = [b' '; 32];
    let id = MEDIA_VOLUME_ID.as_bytes();
    label
        .get_mut(..id.len())
        .ok_or("volume identifier longer than its field")?
        .copy_from_slice(id);
    Ok(descriptor.get(..7) == Some(b"\x01CD001\x01".as_slice())
        && descriptor.get(40..72) == Some(label.as_slice()))
}

/// What `check_out` found at `out`, and so what publication may replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admitted {
    /// Nothing: publication never replaces anything.
    Absent,
    /// A td installation ISO `--force` may replace, if it is still this file.
    TdIso { dev: u64, ino: u64 },
}

/// Settle `out` before the build: an existing file is refused unless `--force`
/// names a td installation ISO.
pub(crate) fn check_out(options: &BuildIsoOptions) -> Result<Admitted, String> {
    let staged = staged_path(&options.out)?;
    let raw = options.out.as_os_str().as_encoded_bytes();
    // `Path` drops a trailing `/` or `/.`; both name a directory.
    if raw.ends_with(b"/") || raw.ends_with(b"/.") {
        return Err(format!(
            "{} names a directory, not a file",
            options.out.display()
        ));
    }
    refuse_stale(&staged)?;
    let admitted = match fs::symlink_metadata(&options.out) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Admitted::Absent,
        Err(error) => return Err(format!("inspect {}: {error}", options.out.display())),
        Ok(metadata) if metadata.is_dir() => {
            return Err(format!(
                "{} is a directory; --out names the ISO file",
                options.out.display()
            ));
        }
        Ok(_) if !options.force => {
            return Err(format!(
                "{} already exists; pass --force to replace a td installation ISO",
                options.out.display()
            ));
        }
        Ok(_) if !is_td_iso(&options.out)? => {
            return Err(format!(
                "{} is not a td installation ISO; --force replaces only those",
                options.out.display()
            ));
        }
        Ok(metadata) => Admitted::TdIso {
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
    };
    if let Some(parent) = options.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    Ok(admitted)
}

/// Refuse a staged image this run did not write, so no failure removes it.
/// A refused publication leaves one, and a later run may reuse the pid.
fn refuse_stale(staged: &Path) -> Result<(), String> {
    match fs::symlink_metadata(staged) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect {}: {error}", staged.display())),
        Ok(_) => Err(format!(
            "{} is left from an earlier build-iso; move or remove it",
            staged.display()
        )),
    }
}

/// Move a complete image from `staged` to `out`.
///
/// An admitted-absent destination is published with a no-replacement link, so
/// a file that appeared during the build is kept and this fails. An admitted
/// ISO is replaced by a rename, checked immediately before to be still that
/// file and still a td installation ISO; anything else there is kept. If it
/// is gone, the image is linked as for an absent one. A refused publication
/// keeps the image and names it. The directory is synced so a reported
/// publication survives a crash.
fn publish(staged: &Path, out: &Path, admitted: Admitted) -> Result<(), String> {
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let result = fs::File::open(parent)
        .map_err(|error| format!("open {}: {error}", parent.display()))
        .and_then(|directory| {
            let linked = match admitted {
                Admitted::Absent => None,
                Admitted::TdIso { dev, ino } => {
                    // Serializes build-iso replacements here, so two `--force`
                    // runs cannot both pass the check and overwrite each other.
                    // A process that ignores the lock is not excluded: rename(2)
                    // cannot make the check and the replacement one step. The
                    // no-replacement link takes no lock, since NFS refuses an
                    // exclusive lock on a directory.
                    directory
                        .lock()
                        .map_err(|error| format!("lock {}: {error}", parent.display()))?;
                    still_admitted(out, dev, ino)?
                }
            };
            match linked {
                None => fs::hard_link(staged, out)
                    .map_err(|error| format!("publish {}: {error}", out.display()))?,
                Some(true) => fs::rename(staged, out)
                    .map_err(|error| format!("replace {}: {error}", out.display()))?,
                Some(false) => {
                    return Err(format!(
                        "{} changed during the build; it is kept",
                        out.display()
                    ))
                }
            }
            Ok(directory)
        });
    let directory = result
        .map_err(|error| format!("{error}; the new image is kept at {}", staged.display()))?;
    match fs::remove_file(staged) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", staged.display())),
    }
    directory
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", parent.display()))
}

/// Whether `out` is still the admitted td installation ISO; `None` if gone.
fn still_admitted(out: &Path, dev: u64, ino: u64) -> Result<Option<bool>, String> {
    let current = match fs::symlink_metadata(out) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {error}", out.display())),
    };
    Ok(Some(
        (current.dev(), current.ino()) == (dev, ino) && is_td_iso(out)?,
    ))
}

/// `lock` is the ladder lock, held while the ISO streams payloads out of the
/// ladder store.
pub(crate) fn run(
    runner: &RecipeCheckRunner,
    lock: fs::File,
    options: &BuildIsoOptions,
    admitted: Admitted,
) -> Result<(), String> {
    println!(
        "   [build-iso] building the td distro (system-x86-64). An unchanged tree is\n            \
         reused whole; a cold tree climbs the whole ladder from stage0.\n"
    );
    let (kernel, selector, deployment) = build_system(runner)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    let trust = RunTrust::generate()?;
    let LiveMedium {
        id,
        selector: live,
        payloads,
    } = live_medium(&selector, &deployment, &scratch.dir, &trust)?;

    let staged = staged_path(&options.out)?;
    refuse_stale(&staged)?;
    println!("   [build-iso] writing deployment {id}");
    if let Err(error) = media::write_image_with_payloads(&staged, &kernel, &live, &payloads) {
        // The writer can fail after publishing `staged`, at its final sync.
        // It refuses an existing name and none was there, so this removes
        // only its own publication.
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    drop(lock);
    publish(&staged, &options.out, admitted)?;
    println!(
        "   [build-iso] wrote {} (deployment {id})\n            \
         boot it in QEMU with ./test-iso {}, or write it to a USB stick",
        options.out.display(),
        options.out.display()
    );
    Ok(())
}

/// What a live medium carries besides its kernel: the signed deployment's ID,
/// the live selector, and the payloads under their medium names.
pub(crate) struct LiveMedium {
    pub(crate) id: String,
    pub(crate) selector: PathBuf,
    pub(crate) payloads: Vec<(&'static str, PathBuf)>,
}

/// Sign the verified store deployment with `trust` and provision the live
/// selector for it, in `dir`. Only the manifest is copied, so its signature
/// can be written beside it; the other payloads stay in the store.
pub(crate) fn live_medium(
    selector: &VerifiedSelector,
    deployment: &Path,
    dir: &Path,
    trust: &RunTrust,
) -> Result<LiveMedium, String> {
    let signed = dir.join("signed");
    fs::create_dir(&signed).map_err(|error| format!("create {}: {error}", signed.display()))?;
    fs::copy(deployment.join(MANIFEST_NAME), signed.join(MANIFEST_NAME))
        .map_err(|error| format!("stage the deployment manifest: {error}"))?;
    trust.sign_deployment(&signed)?;
    let id = crate::sha256::sha256_file(&signed.join(MANIFEST_NAME))
        .map_err(|error| format!("hash the deployment manifest: {error}"))?;
    // `build_system` verified the payloads against the store manifest; the
    // signed copy must be those bytes, or the signature vouches for another.
    let store_id = crate::sha256::sha256_file(&deployment.join(MANIFEST_NAME))
        .map_err(|error| format!("hash the store manifest: {error}"))?;
    if store_id != id {
        return Err("the deployment manifest changed while it was signed".into());
    }
    let selector = provision_live_selector(selector, dir, trust)?;
    let payloads = MEDIA_DEPLOYMENT_FILES
        .iter()
        .map(|(iso, name)| {
            let from = if *name == MANIFEST_NAME || *name == MANIFEST_SIG_NAME {
                &signed
            } else {
                deployment
            };
            (*iso, from.join(name))
        })
        .collect();
    Ok(LiveMedium {
        id,
        selector,
        payloads,
    })
}

/// A sibling of `out`, so publishing is a link in one directory.
fn staged_path(out: &Path) -> Result<PathBuf, String> {
    let name = out
        .file_name()
        .ok_or_else(|| format!("{} names no file", out.display()))?;
    let mut staged = std::ffi::OsString::from(".");
    staged.push(name);
    staged.push(format!(".{}.partial", std::process::id()));
    Ok(out.with_file_name(staged))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_default_and_refuse_ambiguity() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            BuildIsoOptions {
                out: PathBuf::from(DEFAULT_OUT),
                force: false
            }
        );
        assert_eq!(
            parse_args(&args(&["--out", "x.iso", "--force"])).unwrap(),
            BuildIsoOptions {
                out: PathBuf::from("x.iso"),
                force: true
            }
        );
        for bad in [
            vec!["--out"],
            vec!["--out", "a", "--out", "b"],
            vec!["x.iso"],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn force_replaces_only_a_td_installation_iso() {
        let dir = std::env::temp_dir().join(format!("td-build-iso-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let out = dir.join("out.iso");
        let options = |force| BuildIsoOptions {
            out: out.clone(),
            force,
        };
        assert_eq!(check_out(&options(false)).unwrap(), Admitted::Absent);

        fs::write(&out, b"precious").unwrap();
        assert!(check_out(&options(false)).is_err());
        assert!(check_out(&options(true)).is_err(), "not an ISO");

        let mut iso = vec![0u8; 17 * 2048];
        iso[16 * 2048..16 * 2048 + 7].copy_from_slice(b"\x01CD001\x01");
        iso[16 * 2048 + 40..16 * 2048 + 72].fill(b' ');
        iso[16 * 2048 + 40..16 * 2048 + 50].copy_from_slice(b"TD_INSTALL");
        fs::write(&out, &iso).unwrap();
        assert!(check_out(&options(false)).is_err());
        assert!(matches!(
            check_out(&options(true)).unwrap(),
            Admitted::TdIso { .. }
        ));

        iso[16 * 2048 + 40..16 * 2048 + 50].copy_from_slice(b"OTHER_DISC");
        fs::write(&out, &iso).unwrap();
        assert!(check_out(&options(true)).is_err(), "another volume label");

        fs::remove_file(&out).unwrap();
        std::os::unix::fs::symlink("/dev/null", &out).unwrap();
        assert!(check_out(&options(true)).is_err(), "a symlink");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn staging_is_a_hidden_sibling() {
        let staged = staged_path(Path::new("dist/td.iso")).unwrap();
        assert_eq!(staged.parent(), Some(Path::new("dist")));
        assert!(staged
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".td.iso."));
        assert!(staged_path(Path::new("/")).is_err());
    }

    fn td_iso(tag: u8) -> Vec<u8> {
        let mut iso = vec![tag; 17 * 2048];
        iso[16 * 2048..16 * 2048 + 7].copy_from_slice(b"\x01CD001\x01");
        iso[16 * 2048 + 40..16 * 2048 + 72].fill(b' ');
        iso[16 * 2048 + 40..16 * 2048 + 50].copy_from_slice(b"TD_INSTALL");
        iso
    }

    #[test]
    fn publication_replaces_only_the_admitted_iso() {
        let dir = std::env::temp_dir().join(format!("td-build-iso-pub-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let out = dir.join("out.iso");
        let staged = staged_path(&out).unwrap();
        let options = BuildIsoOptions {
            out: out.clone(),
            force: true,
        };

        // Absent at admission: a file that appears is kept.
        let admitted = check_out(&options).unwrap();
        fs::write(&staged, td_iso(1)).unwrap();
        fs::write(&out, b"appeared").unwrap();
        assert!(publish(&staged, &out, admitted).is_err());
        assert_eq!(fs::read(&out).unwrap(), b"appeared");
        assert_eq!(
            fs::read(&staged).unwrap(),
            td_iso(1),
            "a refused image is kept"
        );

        // Absent and still absent: published.
        fs::remove_file(&out).unwrap();
        fs::write(&staged, td_iso(2)).unwrap();
        publish(&staged, &out, admitted).unwrap();
        assert_eq!(fs::read(&out).unwrap(), td_iso(2));
        assert!(!staged.exists());

        // The admitted ISO is replaced.
        let admitted = check_out(&options).unwrap();
        fs::write(&staged, td_iso(3)).unwrap();
        publish(&staged, &out, admitted).unwrap();
        assert_eq!(fs::read(&out).unwrap(), td_iso(3));

        // An ISO swapped in for the admitted one is kept.
        // Renamed over while the admitted file still exists, so the inode
        // cannot be reused.
        let admitted = check_out(&options).unwrap();
        let swapped = dir.join("swapped.iso");
        fs::write(&swapped, td_iso(4)).unwrap();
        fs::rename(&swapped, &out).unwrap();
        fs::write(&staged, td_iso(5)).unwrap();
        assert!(publish(&staged, &out, admitted).is_err());
        assert_eq!(fs::read(&out).unwrap(), td_iso(4));

        // A refused image left staged refuses the next run before its build.
        let error = check_out(&options).unwrap_err();
        assert!(error.contains("left from an earlier build-iso"), "{error}");
        assert_eq!(fs::read(&staged).unwrap(), td_iso(5));
        fs::remove_file(&staged).unwrap();

        // The admitted file, no longer an ISO, is kept.
        let admitted = check_out(&options).unwrap();
        fs::write(&out, b"overwritten in place").unwrap();
        fs::write(&staged, td_iso(6)).unwrap();
        let error = publish(&staged, &out, admitted).unwrap_err();
        assert!(error.contains(&staged.display().to_string()), "{error}");
        assert_eq!(fs::read(&out).unwrap(), b"overwritten in place");
        fs::remove_file(&staged).unwrap();

        // The admitted ISO removed during the build: published as if absent.
        fs::write(&out, td_iso(7)).unwrap();
        let admitted = check_out(&options).unwrap();
        fs::remove_file(&out).unwrap();
        fs::write(&staged, td_iso(8)).unwrap();
        publish(&staged, &out, admitted).unwrap();
        assert_eq!(fs::read(&out).unwrap(), td_iso(8));
        assert!(!staged.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_destination_naming_no_file_is_refused_before_the_build() {
        let dir = std::env::temp_dir().join(format!("td-build-iso-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let existing = dir.to_str().unwrap().to_string();
        let dotted = format!("{existing}/x.iso/.");
        for out in ["/", "..", "newdir/", ".", &dotted, &existing] {
            for force in [false, true] {
                let options = BuildIsoOptions {
                    out: PathBuf::from(out),
                    force,
                };
                let error = check_out(&options).unwrap_err();
                assert!(!error.contains("--force"), "{out}: {error}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
