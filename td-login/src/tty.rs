//! Handing the terminal to the user `login` logs in: `chown` to uid:gid and
//! `chmod 0600`, so the next session cannot read the previous user's terminal.
//!
//! This is the sharpest edge in the program (THREAT-MODEL.md §6). Whoever execs
//! `login` chooses what is on fd 0, and a `login` that chowns "whatever fd 0 is
//! called" hands the target user ownership of any file the caller could open.
//! So the object is identified five ways before anything is changed, and a
//! failed identification means "do not chown", never "do not log in" — refusing
//! the session would brick the console over a cosmetic property.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The kernel's answer to "what is fd 0". No path is ever accepted from argv.
const STDIN_LINK: &str = "/proc/self/fd/0";
const STAT: &str = "/proc/self/stat";

/// `S_IFMT`/`S_IFCHR`: the file-type bits of `st_mode` and the character-device
/// value. `FileTypeExt::is_char_device` says the same thing, but the raw mode is
/// what `set_permissions` below is about to overwrite, so both come from one
/// `stat` rather than two views that could disagree.
const S_IFMT: u32 = 0o170_000;
const S_IFCHR: u32 = 0o020_000;

/// The mode a login terminal gets: owner read/write, nothing for anyone else.
const TTY_MODE: u32 = 0o600;

/// The permission bits of an `st_mode`, i.e. what `chmod(2)` acts on.
const MODE_BITS: u32 = 0o7777;

/// Give the terminal on fd 0 to `uid`:`gid`. `Ok(Some(path))` is a terminal
/// handed over, `Ok(None)` a caller with no controlling terminal (a `login` run
/// from a script — nothing to hand over and nothing wrong), `Err` a refusal the
/// caller reports as a warning and carries on from.
pub fn hand_over(uid: u32, gid: u32) -> Result<Option<PathBuf>, String> {
    let Some((path, was)) = controlling_terminal()? else {
        return Ok(None);
    };
    fs::set_permissions(&path, fs::Permissions::from_mode(TTY_MODE))
        .map_err(|e| format!("cannot chmod {} to {TTY_MODE:o}: {e}", path.display()))?;
    if let Err(e) = std::os::unix::fs::chown(&path, Some(uid), Some(gid)) {
        // The chmod took and the chown did not, so the terminal is now 0600 and
        // still owned by root: the session about to start cannot read or write
        // its own console. Put the mode back, so the warning login prints is
        // "nothing was handed over" rather than "the console is gone". A failure
        // to restore is appended rather than swallowed -- that IS the dead
        // console, and it is the operator's only notice of it.
        let mut why = format!("cannot chown {} to {uid}:{gid}: {e}", path.display());
        if let Err(back) = fs::set_permissions(&path, fs::Permissions::from_mode(was & MODE_BITS)) {
            why.push_str(&format!(
                "; and mode {:o} could not be restored ({back}), so this terminal is \
                 unusable by anyone but root",
                was & MODE_BITS
            ));
        }
        return Err(why);
    }
    Ok(Some(path))
}

/// Identify fd 0 as this process's controlling terminal, or refuse.
///
/// The tests below cover the two decidable halves — the name rules and the
/// `/proc/self/stat` parse — separately; this function is the sequencing, which
/// needs a live `/proc` and a terminal to exercise and is proven on the image
/// instead (a session whose terminal is not 0600 root-less is visible there).
/// Returns the terminal's path AND the mode it had, so a half-applied hand-over
/// can be undone.
fn controlling_terminal() -> Result<Option<(PathBuf, u32)>, String> {
    // 1. The name, from the kernel rather than from argv.
    let path = fs::read_link(STDIN_LINK)
        .map_err(|e| format!("cannot resolve {STDIN_LINK}: {e} (is /proc mounted?)"))?;
    // 2. It must LOOK like a device node and nothing else.
    check_name(&path)?;
    // 3. The OPEN FILE must be a character device. A regular file, directory,
    //    socket or pipe on fd 0 stops here, whatever it is called.
    let open = fs::metadata(STDIN_LINK)
        .map_err(|e| format!("cannot stat {STDIN_LINK}: {e}"))?;
    if open.mode() & S_IFMT != S_IFCHR {
        return Err(format!(
            "fd 0 is not a character device (mode {:#o}); not touching {}",
            open.mode(),
            path.display()
        ));
    }
    // 4. The NAME and the OPEN FILE must be the same object. This is what
    //    rejects a name that has been re-pointed since the process opened it.
    let named = fs::metadata(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if (named.dev(), named.ino()) != (open.dev(), open.ino()) {
        return Err(format!(
            "{} is no longer the file open on fd 0; not touching it",
            path.display()
        ));
    }
    // 5. ...and that object must be this process's CONTROLLING terminal. Without
    //    this, `/dev/null` — a character device with a perfectly good name under
    //    /dev — would pass every check above and be handed to the user.
    let ctty = controlling_device()?;
    if ctty == 0 {
        return Ok(None);
    }
    if ctty != named.rdev() {
        return Err(format!(
            "{} (device {:#x}) is not the controlling terminal (device {ctty:#x}); \
             not touching it",
            path.display(),
            named.rdev()
        ));
    }
    Ok(Some((path, named.mode())))
}

/// The path must be `/dev/<name>[/<name>…]` with every component a plain name.
/// `..` is the one that matters: `/dev/../etc/shadow` starts with `/dev/` and
/// names something else entirely.
fn check_name(path: &Path) -> Result<(), String> {
    let Some(text) = path.to_str() else {
        return Err(format!("fd 0 resolves to a non-UTF-8 path: {path:?}"));
    };
    let Some(rest) = text.strip_prefix("/dev/") else {
        return Err(format!("fd 0 resolves to {text:?}, which is not under /dev/"));
    };
    if rest.is_empty() {
        return Err("fd 0 resolves to /dev/ itself".into());
    }
    for component in rest.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "fd 0 resolves to {text:?}, which is not a plain /dev path"
            ));
        }
    }
    Ok(())
}

fn controlling_device() -> Result<u64, String> {
    let text = fs::read_to_string(STAT).map_err(|e| format!("cannot read {STAT}: {e}"))?;
    tty_nr(&text).ok_or_else(|| format!("{STAT}: cannot read the controlling-terminal field"))
}

/// `/proc/<pid>/stat` field 7 (`tty_nr`), the controlling terminal's device
/// number — 0 when there is none.
///
/// Field 2 is the executable name in parentheses and may itself contain spaces
/// and parentheses, so the fields are counted from the LAST `)` rather than by
/// splitting the whole line. That is what `procps` does and the only parse that
/// survives a program called `sh) 0 0 0 99999`.
fn tty_nr(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let after = stat.get(close + 1..)?;
    // After the comm field the next tokens are state(3), ppid(4), pgrp(5),
    // session(6), tty_nr(7) — so tty_nr is the 5th token here.
    let value = after.split_whitespace().nth(4)?;
    value.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn only_plain_paths_under_dev_are_accepted() {
        for good in ["/dev/ttyS0", "/dev/tty1", "/dev/pts/0", "/dev/vc/1"] {
            assert!(check_name(Path::new(good)).is_ok(), "{good} should pass");
        }
        for bad in [
            "/dev/../etc/shadow",
            "/dev/./ttyS0",
            "/dev//ttyS0",
            "/dev/",
            "/dev",
            "/etc/shadow",
            "/td/store/abc-busybox/bin/busybox",
            "ttyS0",
            "",
        ] {
            assert!(
                check_name(Path::new(bad)).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    /// The `comm` field is attacker-influenced (it is the program's own name),
    /// so the parse must survive a name built to look like more fields.
    #[test]
    fn tty_nr_is_counted_from_the_last_close_paren() {
        // A plain line: pid comm state ppid pgrp session tty_nr ...
        assert_eq!(tty_nr("412 (sh) S 411 412 412 34816 412 4194304").unwrap(), 34816);
        // No controlling terminal.
        assert_eq!(tty_nr("2 (kthreadd) S 0 0 0 0 -1 2129984").unwrap(), 0);
        // A comm containing spaces AND parens and a decoy field run.
        assert_eq!(
            tty_nr("7 (evil ) S 1 1 1 99999) S 411 412 412 34816 412").unwrap(),
            34816
        );
        assert!(tty_nr("no parens here").is_none());
        assert!(tty_nr("412 (sh) S 411").is_none());
        assert!(tty_nr("412 (sh) S 411 412 412 nope 412").is_none());
    }

    /// The mode constants, since one wrong bit is the difference between a
    /// terminal only its owner can read and one anybody can.
    #[test]
    fn the_terminal_mode_is_owner_only() {
        assert_eq!(TTY_MODE, 0o600);
        assert_eq!(S_IFCHR & S_IFMT, S_IFCHR);
        // A directory's mode must not classify as a character device.
        assert_ne!(0o040_755 & S_IFMT, S_IFCHR);
        assert_eq!(0o020_620 & S_IFMT, S_IFCHR);
    }
}
