//! `chmod` and `chown` — the mode and ownership the boot scripts set.
//!
//! Both take the RESTRICTED forms td actually issues: an octal mode, and a
//! numeric `UID:GID`. Symbolic modes (`u+x`) and name lookups are refused rather
//! than half-supported, because a mode this misparsed is a permission silently
//! wrong on a path the boot just created.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn usage_chmod() -> String {
    "usage: chmod OCTAL-MODE PATH...".to_string()
}

fn usage_chown() -> String {
    "usage: chown UID:GID PATH...".to_string()
}

/// Octal only, and 4 digits max — `chmod 07777` is the whole permission word.
/// Refused rather than accepted-and-truncated: a symbolic operand parsed as 0
/// would strip every bit off a path the boot depends on.
fn parse_mode(spec: &str) -> Result<u32, String> {
    if spec.is_empty() || spec.len() > 4 || !spec.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err(format!("invalid mode '{spec}' (octal only)\n{}", usage_chmod()));
    }
    u32::from_str_radix(spec, 8).map_err(|e| format!("invalid mode '{spec}': {e}"))
}

/// `UID:GID`, both numeric. A NAME would need `/etc/passwd`, which does not
/// exist in the initramfs and which this deliberately does not parse; every td
/// caller passes the numbers it already has.
fn parse_owner(spec: &str) -> Result<(u32, u32), String> {
    let Some((u, g)) = spec.split_once(':') else {
        return Err(format!("owner '{spec}' is not UID:GID\n{}", usage_chown()));
    };
    let uid: u32 = u
        .parse()
        .map_err(|_| format!("uid '{u}' is not a number\n{}", usage_chown()))?;
    let gid: u32 = g
        .parse()
        .map_err(|_| format!("gid '{g}' is not a number\n{}", usage_chown()))?;
    Ok((uid, gid))
}

fn split(args: &[String], usage: fn() -> String) -> Result<(&str, Vec<&str>), String> {
    let mut rest: Vec<&str> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unrecognised option '{a}'\n{}", usage()));
        }
        rest.push(a.as_str());
    }
    let Some((spec, paths)) = rest.split_first() else {
        return Err(usage());
    };
    if paths.is_empty() {
        return Err(usage());
    }
    Ok((spec, paths.to_vec()))
}

pub fn chmod(args: &[String]) -> Result<u8, String> {
    let (spec, paths) = split(args, usage_chmod)?;
    let mode = parse_mode(spec)?;
    let mut status = 0u8;
    for p in paths {
        if let Err(e) = std::fs::set_permissions(Path::new(p), PermissionsExt::from_mode(mode)) {
            crate::emit_err(&format!("chmod: {p}: {e}\n"));
            status = 1;
        }
    }
    Ok(status)
}

pub fn chown(args: &[String]) -> Result<u8, String> {
    let (spec, paths) = split(args, usage_chown)?;
    let (uid, gid) = parse_owner(spec)?;
    let mut status = 0u8;
    for p in paths {
        // `chown`, not `lchown`: the busybox call this replaces followed
        // symlinks, and the homes it retargets are real directories.
        if let Err(e) = std::os::unix::fs::chown(Path::new(p), Some(uid), Some(gid)) {
            crate::emit_err(&format!("chown: {p}: {e}\n"));
            status = 1;
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    /// The four modes td issues, and the shapes that must NOT be guessed at.
    #[test]
    fn only_octal_modes_are_accepted() {
        for (spec, want) in [("0755", 0o755), ("0700", 0o700), ("0644", 0o644), ("600", 0o600)] {
            assert_eq!(parse_mode(spec), Ok(want), "{spec}");
        }
        for bad in ["u+x", "", "8", "0o755", "07555", "-rwx", "a=r"] {
            assert!(
                parse_mode(bad).is_err(),
                "'{bad}' was accepted as a mode; a misparse silently mis-permissions a boot path"
            );
        }
    }

    /// Numeric only. A name would need /etc/passwd, which the initramfs has not
    /// got — and quietly resolving to 0 would hand root the caller's files.
    #[test]
    fn only_numeric_owners_are_accepted() {
        assert_eq!(parse_owner("0:0"), Ok((0, 0)));
        assert_eq!(parse_owner("1000:1000"), Ok((1000, 1000)));
        for bad in ["root:root", "1000", "1000:", ":1000", "", "1000:1000:1", "-1:0"] {
            assert!(parse_owner(bad).is_err(), "'{bad}' was accepted as an owner");
        }
    }

    #[test]
    fn an_operand_is_required() {
        assert!(chmod(&args(&["0755"])).is_err(), "a mode with no path is a usage error");
        assert!(chown(&args(&["0:0"])).is_err(), "an owner with no path is a usage error");
        assert!(chmod(&args(&[])).is_err());
        assert!(chown(&args(&[])).is_err());
    }

    /// ...and the mode is really applied.
    #[test]
    fn chmod_sets_the_mode_it_was_given() {
        let d = std::env::temp_dir().join(format!("td-util-chmod-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("f");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(chmod(&args(&["0600", &f.to_string_lossy()])), Ok(0));
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "chmod did not apply the mode");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A missing path is a status, not a stop: `chmod 0755 a b` must still reach b.
    #[test]
    fn a_missing_path_sets_the_status() {
        let missing = format!("/nonexistent/td-util-{}", std::process::id());
        assert_eq!(chmod(&args(&["0755", &missing])), Ok(1));
        assert_eq!(chown(&args(&["0:0", &missing])), Ok(1));
    }
}
