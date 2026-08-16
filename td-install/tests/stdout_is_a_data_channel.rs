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
