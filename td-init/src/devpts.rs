//! `devpts` — mount td's Unix98 pty instance and point `/dev/ptmx` at it.
//!
//! td-term opens `/dev/ptmx` for every terminal it starts, and nothing on the
//! real root mounts devpts, so this is the applet that makes a pty allocatable
//! at all. DESIGN.md §12 specifies the sequence and the reasoning; this is one
//! program rather than four sysinit lines so that no part of it rests on a
//! uutils binary being present at an absolute path.
//!
//! It does NOT call `sys::mount`: the mount goes through the `mount` applet as
//! the argv an inittab line would have written, which keeps flag composition in
//! `mount.rs` where the crate's confinement tests require it.

use crate::mount;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

const DIR: &str = "/dev/pts";
const PTMX: &str = "/dev/ptmx";
const MOUNTS: &str = "/proc/mounts";

/// Relative, so the link resolves inside whatever `/dev` it is read through.
/// This is the setup the kernel's own devpts documentation describes. It is
/// NOT that a `/dev/ptmx` device node would allocate from some other instance:
/// modern kernels resolve a `pts` directory beside the node and use that mount.
/// The link states this instance explicitly instead of resting on that lookup.
const LINK_TARGET: &str = "pts/ptmx";

/// `gid=5` is the image's `tty` group and `mode=0620` is the slave's: owner
/// read/write, tty group WRITE. Group-write is the convention's point — it is
/// how anything reaches a terminal it does not own — and it is not a
/// relaxation, since devpts would otherwise serve slaves 0600 owned by the
/// opener's own group. `ptmxmode=0666` is the one whose absence stops td-term
/// dead: an instance's own `ptmx` is mode 0000 by default.
///
/// `newinstance` asks for a private namespace. Kernels new enough to matter
/// give every devpts mount its own instance and accept the token as a no-op,
/// which is why it is excluded from the readback below.
///
/// Deliberately NOT `nosuid,noexec`, which systemd and util-linux both pass:
/// devpts's root offers no `create` or `mknod`, so there is no file to be
/// setuid or executable, and td's other pseudo-filesystem lines are bare. A
/// considered divergence rather than an omission.
const OPTIONS: &str = "newinstance,ptmxmode=0666,mode=0620,gid=5";

/// What `ptmxmode=0666` must have produced, as `stat` reports it.
const PTMX_MODE: u32 = 0o666;

fn usage() -> String {
    "usage: devpts        (mounts /dev/pts and relinks /dev/ptmx)".to_string()
}

/// The mount, as the argv `mount` parses.
pub(crate) fn mount_argv() -> Vec<String> {
    ["-t", "devpts", "-o", OPTIONS, "devpts", DIR]
        .iter()
        .map(|word| (*word).to_string())
        .collect()
}

/// The options as `/proc/mounts` will spell them, derived from the ones asked
/// for rather than restated beside them. devpts prints its modes with `%03o`,
/// so the `mode=0620` this passes comes back as `mode=620` — a readback
/// written in the mount's own spelling matches nothing on a correct machine.
fn expected_options() -> Vec<String> {
    OPTIONS
        .split(',')
        .filter(|option| *option != "newinstance")
        .map(|option| match option.split_once('=') {
            Some((key, value)) if key.ends_with("mode") => {
                let printed = value.trim_start_matches('0');
                format!("{key}={}", if printed.is_empty() { "0" } else { printed })
            }
            _ => option.to_string(),
        })
        .collect()
}

/// The option field of the devpts mount at `target`, or `None` if there is no
/// devpts mounted there. Fields are split on whitespace, so the answer cannot
/// come from a substring of some other mount's line.
fn devpts_options(table: &str, target: &str) -> Option<String> {
    for line in table.lines() {
        let mut fields = line.split_whitespace();
        let found = match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some(_), Some(mounted), Some("devpts"), Some(options))
                if mounted == target =>
            {
                options
            }
            _ => continue,
        };
        return Some(found.to_string());
    }
    None
}

/// Proof that the instance came up as asked. `relink_ptmx` requires one, so
/// the ordering DESIGN.md gives its own paragraph — verify, then relink — is
/// the compiler's to enforce rather than a comment's to request.
#[derive(Debug)]
struct Verified;

/// Refuse unless every option took and the instance's own `ptmx` is there.
///
/// Each option is checked as a whole comma-separated token, so `mode=620`
/// cannot be satisfied by `ptmxmode=620`. An option devpts does not know makes
/// the mount fail outright, so what this catches is a known option that took a
/// DIFFERENT value than asked — the case nothing else distinguishes, since the
/// machine then boots and looks healthy until a pty is opened.
fn verify(table: &str, dir: &Path) -> Result<Verified, String> {
    let target = dir.to_string_lossy();
    let options = devpts_options(table, &target)
        .ok_or_else(|| format!("no devpts is mounted on {target} after mounting one"))?;
    for expected in expected_options() {
        if !options.split(',').any(|option| option == expected) {
            return Err(format!(
                "{target} is mounted {options}, without {expected}: the mount took the \
                 filesystem but not that option, and nothing distinguishes the difference \
                 until a pty is opened"
            ));
        }
    }
    let ptmx = dir.join("ptmx");
    let stat = fs::metadata(&ptmx)
        .map_err(|e| format!("{}: {e} — devpts mounted without its own ptmx", ptmx.display()))?;
    if !stat.file_type().is_char_device() {
        return Err(format!("{} is not a character device", ptmx.display()));
    }
    let mode = stat.permissions().mode() & 0o7777;
    if mode != PTMX_MODE {
        return Err(format!(
            "{} is mode {mode:04o}, not {PTMX_MODE:04o}",
            ptmx.display()
        ));
    }
    Ok(Verified)
}

/// Replace whatever `/dev/ptmx` is with the relative symlink, by rename so the
/// node is never absent. Unlinking and then creating would leave a boot with no
/// `/dev/ptmx` at all if the second step failed.
fn relink_ptmx(ptmx: &Path, _verified: Verified) -> Result<(), String> {
    let staging = match ptmx.file_name().and_then(|name| name.to_str()) {
        Some(name) => ptmx.with_file_name(format!("{name}.td-new")),
        None => return Err(format!("{}: has no file name", ptmx.display())),
    };
    // A leftover from an interrupted earlier run is not this run's answer.
    match fs::remove_file(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", staging.display())),
    }
    std::os::unix::fs::symlink(LINK_TARGET, &staging)
        .map_err(|e| format!("{} -> {LINK_TARGET}: {e}", staging.display()))?;
    if let Err(e) = fs::rename(&staging, ptmx) {
        let _ = fs::remove_file(&staging);
        return Err(format!("{} -> {}: {e}", staging.display(), ptmx.display()));
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<u8, String> {
    if let Some(unexpected) = args.first() {
        return Err(format!(
            "devpts takes no arguments, got '{unexpected}'\n{}",
            usage()
        ));
    }
    let dir = Path::new(DIR);
    let table = fs::read_to_string(MOUNTS).map_err(|e| format!("{MOUNTS}: {e}"))?;
    // devpts stacks: mounting a second instance over the first hides every
    // live pty's `/dev/pts/N` while every check still reads healthy, so a
    // second run is refused rather than served.
    if devpts_options(&table, &dir.to_string_lossy()).is_some() {
        return Err(format!(
            "a devpts is already mounted on {}; mounting another would hide every \
             pty the first one is serving",
            dir.display()
        ));
    }
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // `mount` reports every failure as an Err today; this is a guard for the
    // day another module's entry point reports one through its exit code.
    let status = mount::mount(&mount_argv())?;
    if status != 0 {
        return Ok(status);
    }
    let table = fs::read_to_string(MOUNTS).map_err(|e| format!("{MOUNTS}: {e}"))?;
    let verified = verify(&table, dir)?;
    relink_ptmx(Path::new(PTMX), verified)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A devpts line as the kernel actually writes it.
    const REAL: &str = "devpts /dev/pts devpts rw,nosuid,noexec,relatime,gid=5,mode=620,\
                        ptmxmode=666 0 0";

    /// The options are this mount's whole security content, and a mount with a
    /// mistyped one still succeeds, so the argv is pinned word for word.
    #[test]
    fn the_mount_argv_is_the_instance_design_md_specifies() {
        assert_eq!(
            mount_argv(),
            vec![
                "-t".to_string(),
                "devpts".to_string(),
                "-o".to_string(),
                "newinstance,ptmxmode=0666,mode=0620,gid=5".to_string(),
                "devpts".to_string(),
                "/dev/pts".to_string(),
            ]
        );
    }

    /// The derivation exists because the two spellings differ, so it is
    /// checked against a real line rather than against the constant it came
    /// from: `mode=0620` matches nothing a kernel ever writes.
    #[test]
    fn the_readback_uses_the_kernels_spelling_not_the_mounts() {
        let expected = expected_options();
        assert_eq!(expected, vec!["ptmxmode=666", "mode=620", "gid=5"]);
        let found = devpts_options(REAL, "/dev/pts").unwrap();
        for option in &expected {
            assert!(
                found.split(',').any(|f| f == option),
                "{option} is not how the kernel spells it: {found}"
            );
        }
        // `newinstance` is excluded: kernels accept it and echo nothing back,
        // so requiring it would refuse a correct mount.
        assert!(!expected.iter().any(|option| option == "newinstance"));
    }

    #[test]
    fn a_devpts_mounted_elsewhere_is_not_this_one() {
        assert!(devpts_options(REAL, "/dev/pts").is_some());
        assert_eq!(devpts_options(REAL, "/dev/pts2"), None);
        // Right mount point, wrong filesystem.
        assert_eq!(
            devpts_options("tmpfs /dev/pts tmpfs rw 0 0", "/dev/pts"),
            None
        );
        // A path that merely CONTAINS the target is a different mount.
        assert_eq!(
            devpts_options("devpts /run/dev/pts devpts rw,gid=5 0 0", "/dev/pts"),
            None
        );
        // A short line is skipped rather than read past its end.
        assert_eq!(devpts_options("devpts /dev/pts\n", "/dev/pts"), None);
    }

    /// An option that took a different value is the case the whole readback
    /// exists for, and each is refused by name.
    #[test]
    fn an_option_that_took_a_different_value_is_refused_by_name() {
        let dir = Path::new("/dev/pts");
        for (wrong, missing) in [
            ("gid=5,mode=666,ptmxmode=666", "mode=620"),
            ("gid=0,mode=620,ptmxmode=666", "gid=5"),
            ("gid=5,mode=620,ptmxmode=000", "ptmxmode=666"),
        ] {
            let table = format!("devpts /dev/pts devpts rw,{wrong} 0 0");
            let error = verify(&table, dir).unwrap_err();
            assert!(error.contains(missing), "{error}");
        }
        // A value that is a SUBSTRING of another option's does not satisfy it:
        // `ptmxmode=620` must not stand in for `mode=620`.
        let table = "devpts /dev/pts devpts rw,gid=5,ptmxmode=620 0 0";
        let error = verify(table, dir).unwrap_err();
        assert!(error.contains("mode=620"), "{error}");
    }

    #[test]
    fn a_missing_mount_is_named_rather_than_its_options_scanned() {
        let error = verify("", Path::new("/dev/pts")).unwrap_err();
        assert!(error.contains("no devpts is mounted on /dev/pts"), "{error}");
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("td-devpts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The link is RELATIVE, and asserting that against the constant that
    /// defines it would pass just as happily if it were made absolute — while
    /// the image's own boot check reads the literal.
    #[test]
    fn relinking_replaces_whatever_is_there_with_the_relative_link() {
        assert_eq!(LINK_TARGET, "pts/ptmx");
        let dir = scratch("relink");
        let ptmx = dir.join("ptmx");
        fs::write(&ptmx, b"not a symlink").unwrap();
        assert_eq!(relink_ptmx(&ptmx, Verified), Ok(()));
        assert_eq!(fs::read_link(&ptmx).unwrap(), Path::new("pts/ptmx"));
        // Idempotent: a second run replaces the link it made.
        assert_eq!(relink_ptmx(&ptmx, Verified), Ok(()));
        assert_eq!(fs::read_link(&ptmx).unwrap(), Path::new("pts/ptmx"));

        // A kernel that made no node at all still gets the link.
        let absent = dir.join("absent");
        assert_eq!(relink_ptmx(&absent, Verified), Ok(()));
        assert_eq!(fs::read_link(&absent).unwrap(), Path::new("pts/ptmx"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The original must survive a failed replacement, which is the whole
    /// reason this is a rename rather than an unlink and a create.
    #[test]
    fn a_failed_relink_leaves_the_original_in_place() {
        let dir = scratch("failed");
        // A symlink cannot be renamed over a directory, so this is a rename
        // that fails after the staging link was already created.
        let ptmx = dir.join("ptmx");
        fs::create_dir(&ptmx).unwrap();
        let error = relink_ptmx(&ptmx, Verified).unwrap_err();
        assert!(error.contains("ptmx.td-new"), "{error}");
        assert!(ptmx.is_dir(), "a failed relink destroyed what was there");
        assert!(
            fs::symlink_metadata(dir.join("ptmx.td-new")).is_err(),
            "the staging link was left behind"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn devpts_takes_no_arguments() {
        let error = run(&["/dev/pts".to_string()]).unwrap_err();
        assert!(error.contains("takes no arguments"), "{error}");
    }
}
