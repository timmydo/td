//! `td-install`'s stdout carries its own line and nothing else.
//!
//! This is a SUBPROCESS test because the property is about file descriptor 1
//! of the real process. The unit tests hand `run_volume` a `Vec` to write its
//! line into, which is what makes the arithmetic checkable — and is exactly why
//! they cannot see this: a child that inherits fd 1 writes past the `Vec`
//! entirely. The bug that prompted this shipped under a full green unit suite
//! and was caught by the recipe check, an hour away.
//!
//! `mkfs.btrfs` opens with a version banner, so a caller parsing the first line
//! of `td-install volume` read `v7.0` where it wanted a byte offset.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

type Res<T> = Result<T, Box<dyn Error>>;

const BIN: &str = env!("CARGO_BIN_EXE_td-install");
/// The ESP plus the smallest volume `plan` accepts, and no bigger: this is a
/// sparse file, but the copy reads every byte of the volume.
const DISK: u64 = 4 * 1024 * 1024 * 1024;

fn scratch_dir(tag: &str) -> Res<PathBuf> {
    let dir = std::env::temp_dir().join(format!("td-install-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// An executable `mkfs.btrfs` stand-in that CHATTERS on stdout. No host is
/// required to have the real one, and what is under test is the plumbing.
fn noisy_mkfs(dir: &Path) -> Res<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let fake = dir.join("mkfs.btrfs");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'btrfs-progs v7.0'\necho 'Label: td-system'\n",
    )?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    Ok(fake)
}

fn script(path: &Path, body: &str) -> Res<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// A deployment id is 64 lowercase hex, and `td-install` reads the one its
/// child prints back against the tree it claims to have written — so a stand-in
/// must publish something for the command to succeed at all.
const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn publishing_td_boot(path: &Path, body: &str) -> Res<()> {
    script(
        path,
        &format!("#!/bin/sh\n{body}mkdir -p \"$2/td/deployments/{ID}\"\necho {ID}\n"),
    )
}

#[test]
fn a_childs_banner_stays_out_of_the_reported_line() -> Res<()> {
    let dir = scratch_dir("stdout")?;
    let disk = dir.join("disk.img");
    std::fs::File::create(&disk)?.set_len(DISK)?;

    let layout = Command::new(BIN).arg("layout").arg(&disk).output()?;
    assert!(
        layout.status.success(),
        "layout failed: {}",
        String::from_utf8_lossy(&layout.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&layout.stdout).lines().count(),
        1,
        "layout wrote more than its own line"
    );

    let mkfs = noisy_mkfs(&dir)?;
    let volume = Command::new(BIN)
        .arg("volume")
        .arg(&disk)
        .arg(&mkfs)
        .arg(&dir)
        .output()?;
    assert!(
        volume.status.success(),
        "volume failed: {}",
        String::from_utf8_lossy(&volume.stderr)
    );
    let out = String::from_utf8(volume.stdout)?;
    assert_eq!(
        out.lines().count(),
        1,
        "stdout carried more than the reported line: {out:?}"
    );
    assert!(
        !out.contains("btrfs-progs"),
        "the child's banner reached stdout: {out:?}"
    );
    // ...and it is not merely SUPPRESSED: it goes to stderr, so a failing mkfs
    // still says what went wrong.
    let err = String::from_utf8_lossy(&volume.stderr);
    assert!(
        err.contains("btrfs-progs"),
        "the child's output was discarded rather than redirected: {err:?}"
    );
    // The line parses as three numbers, which is what a caller reads.
    let fields: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        fields.len(),
        3,
        "the reported line is not <off> <len> <written>: {out:?}"
    );
    for field in &fields {
        assert!(
            field.parse::<u64>().is_ok(),
            "{field:?} is not a number — a banner line would land here: {out:?}"
        );
    }

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// A RELATIVE `<scratch-dir>` publishes, as it formats.
///
/// `td-boot` requires an absolute volume root, and the staging path is built by
/// joining onto the scratch argument — so without resolving it, `volume
/// ./scratch` succeeded and `volume ./scratch <td-boot> …` failed, a difference
/// between the two forms that nothing about either announces. A SUBPROCESS test
/// because the relative path has to be relative to something: `Command` takes a
/// working directory, where changing this process's would race every other test
/// in the binary.
#[test]
fn a_relative_scratch_directory_still_reaches_td_boot() -> Res<()> {
    let dir = scratch_dir("relative")?;
    let disk = dir.join("disk.img");
    std::fs::File::create(&disk)?.set_len(DISK)?;
    let layout = Command::new(BIN).arg("layout").arg(&disk).output()?;
    assert!(
        layout.status.success(),
        "layout failed: {}",
        String::from_utf8_lossy(&layout.stderr)
    );

    let mkfs = noisy_mkfs(&dir)?;
    let td_boot = dir.join("td-boot");
    // Records the root it was handed, so the test can require an ABSOLUTE one
    // rather than merely require that the command succeeded — td-boot is a
    // stand-in here, and a stand-in does not enforce what the real one does.
    publishing_td_boot(
        &td_boot,
        "printf '%s' \"$2\" > \"$(dirname \"$0\")/root\"\n",
    )?;
    let volume = Command::new(BIN)
        .current_dir(&dir)
        .arg("volume")
        .arg(&disk)
        .arg(&mkfs)
        .arg(".")
        .arg(&td_boot)
        .arg(dir.join("deployment"))
        .arg(dir.join("key.pub"))
        .output()?;
    assert!(
        volume.status.success(),
        "a relative scratch directory failed the publish: {}",
        String::from_utf8_lossy(&volume.stderr)
    );
    let root = std::fs::read_to_string(dir.join("root"))?;
    assert!(
        Path::new(&root).is_absolute(),
        "td-boot was handed a relative volume root, which it refuses: {root:?}"
    );
    assert!(
        root.ends_with("td-volume-root"),
        "the root was resolved to somewhere other than the staging tree: {root:?}"
    );

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// The three staged directories carry 0755 whatever the umask was.
///
/// `mkfs.btrfs --rootdir` copies a staging directory's mode into the filesystem
/// verbatim, so these are baked onto the installed machine — and `td/boot`
/// holds the `current`/`previous` symlinks the boot path follows. Under `umask
/// 000` they were created 0777, world-writable on the installed system, and
/// nothing downstream pins them: td-boot's `require_real_directory` asks only
/// whether they are directories.
///
/// A SUBPROCESS test because a umask is per-PROCESS: setting one in-process
/// would apply to every other test in the binary, which run as threads beside
/// it. `sh -c 'umask 000; exec …'` scopes it to the child — and `umask 000` is
/// the hostile direction, since the fix must LOOSEN what the umask tightened as
/// well as tighten what it left open.
#[test]
fn the_staged_directories_do_not_take_the_ambient_umask() -> Res<()> {
    for mask in ["000", "077"] {
        let dir = scratch_dir(&format!("umask{mask}"))?;
        let disk = dir.join("disk.img");
        std::fs::File::create(&disk)?.set_len(DISK)?;
        let layout = Command::new(BIN).arg("layout").arg(&disk).output()?;
        assert!(layout.status.success(), "layout failed under umask {mask}");

        let mkfs = noisy_mkfs(&dir)?;
        let td_boot = dir.join("td-boot");
        publishing_td_boot(&td_boot, "")?;
        let volume = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "umask {mask}; exec \"$0\" volume \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$6\"",
            ))
            .arg(BIN)
            .arg(&disk)
            .arg(&mkfs)
            .arg(&dir)
            .arg(&td_boot)
            .arg(dir.join("deployment"))
            .arg(dir.join("key.pub"))
            .output()?;
        assert!(
            volume.status.success(),
            "volume failed under umask {mask}: {}",
            String::from_utf8_lossy(&volume.stderr)
        );

        use std::os::unix::fs::PermissionsExt;
        let staging = dir.join("td-volume-root");
        for directory in ["td", "td/boot", "td/deployments"] {
            let mode = std::fs::metadata(staging.join(directory))?.permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o755,
                "{directory} came out {mode:#o} under umask {mask}"
            );
        }
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// The SECOND child on this path is `td-boot publish`, whose stdout is the
/// deployment id — and an id is not a byte offset. Here for the same reason the
/// mkfs case is: the unit test hands `run_volume` a `Vec`, so a child given
/// `Stdio::inherit()` writes past it and every in-process assertion stays
/// green. That mutation was applied and observed passing before this existed.
#[test]
fn a_publishing_childs_id_stays_out_of_the_reported_line() -> Res<()> {
    let dir = scratch_dir("publish")?;
    let disk = dir.join("disk.img");
    std::fs::File::create(&disk)?.set_len(DISK)?;

    let layout = Command::new(BIN).arg("layout").arg(&disk).output()?;
    assert!(
        layout.status.success(),
        "layout failed: {}",
        String::from_utf8_lossy(&layout.stderr)
    );

    let mkfs = noisy_mkfs(&dir)?;
    let td_boot = dir.join("td-boot");
    publishing_td_boot(&td_boot, "echo 'published' >&2\n")?;
    let volume = Command::new(BIN)
        .arg("volume")
        .arg(&disk)
        .arg(&mkfs)
        .arg(&dir)
        .arg(&td_boot)
        .arg(dir.join("deployment"))
        .arg(dir.join("key.pub"))
        .output()?;
    assert!(
        volume.status.success(),
        "volume failed: {}",
        String::from_utf8_lossy(&volume.stderr)
    );
    let out = String::from_utf8(volume.stdout)?;
    let fields: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        fields.len(),
        3,
        "stdout carried more than <off> <len> <written>: {out:?}"
    );
    for field in &fields {
        assert!(
            field.parse::<u64>().is_ok(),
            "{field:?} is not a number — the deployment id would land here: {out:?}"
        );
    }
    // ...and the id is not DISCARDED either: a caller that wants it reads
    // stderr, where td-boot's own diagnostics already are.
    let err = String::from_utf8_lossy(&volume.stderr);
    assert!(
        err.contains(ID),
        "the deployment id was dropped rather than redirected: {err:?}"
    );

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
