//! `ln`, `mkdir`, `readlink` and `rm` — the directory and link work the boot
//! scripts do before, and just after, the pivot.

use std::io::Write;
use std::path::Path;

pub fn ln(args: &[String]) -> Result<u8, String> {
    let usage = "usage: ln -s TARGET LINK";
    let mut symbolic = false;
    let mut rest: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-s" => symbolic = true,
            // A HARD link is refused rather than made: `ln` without `-s` is a
            // different object with the same bytes, and every td caller means
            // the symlink. Silently making the other one is not a near miss.
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unrecognised option '{other}'\n{usage}"))
            }
            other => rest.push(other),
        }
    }
    let (Some(target), Some(link)) = (rest.first(), rest.get(1)) else {
        return Err(usage.to_string());
    };
    if !symbolic || rest.len() != 2 {
        return Err(usage.to_string());
    }
    std::os::unix::fs::symlink(target, link).map_err(|e| format!("{link}: {e}"))?;
    Ok(0)
}

pub fn mkdir(args: &[String]) -> Result<u8, String> {
    let usage = "usage: mkdir [-p] DIR...";
    let mut parents = false;
    let mut dirs: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-p" => parents = true,
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unrecognised option '{other}'\n{usage}"))
            }
            other => dirs.push(other),
        }
    }
    if dirs.is_empty() {
        return Err(usage.to_string());
    }
    let mut status = 0u8;
    for d in dirs {
        // `-p` is also what makes an existing directory not an error, which is
        // the property every `mkdir -p` in the boot scripts relies on.
        let r = if parents {
            std::fs::create_dir_all(d)
        } else {
            std::fs::create_dir(d)
        };
        if let Err(e) = r {
            crate::emit_err(&format!("mkdir: {d}: {e}\n"));
            status = 1;
        }
    }
    Ok(status)
}

pub fn readlink(args: &[String]) -> Result<u8, String> {
    let usage = "usage: readlink PATH";
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unrecognised option '{a}'\n{usage}"));
        }
        paths.push(a.as_str());
    }
    let (Some(path), 1) = (paths.first(), paths.len()) else {
        return Err(usage.to_string());
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let status = link_target(path, &mut out)?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(status)
}

/// Write the link target, or write NOTHING and fail.
///
/// Split out so a test can prove the empty half. The scripts compare this output
/// against an expected target (`[ "$(readlink /home)" = /var/home ]`), so printing
/// the path itself — what `readlink -f` would do — makes a plain directory compare
/// equal to a link that was never created.
fn link_target(path: &str, out: &mut impl Write) -> Result<u8, String> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(t) = std::fs::read_link(path) else {
        return Ok(1);
    };
    // RAW BYTES, not `Path::display()`: a target may hold any byte but `/` and
    // NUL, and a lossy decode would print U+FFFD where the link really points —
    // a value the caller then compares or follows.
    match out
        .write_all(t.as_os_str().as_bytes())
        .and_then(|()| out.write_all(b"\n"))
    {
        Ok(()) => Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(0),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

pub fn rm(args: &[String]) -> Result<u8, String> {
    let usage = "usage: rm [-f] [-r|-R] PATH...";
    let mut force = false;
    let mut recursive = false;
    let mut paths: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-f" => force = true,
            "-r" | "-R" => recursive = true,
            "-rf" | "-fr" => {
                force = true;
                recursive = true;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unrecognised option '{other}'\n{usage}"))
            }
            other => paths.push(other),
        }
    }
    if paths.is_empty() {
        // `rm -f` with no operand is success in POSIX; without `-f` it is a
        // usage error. td issues neither, but the asymmetry is cheap to honour.
        return if force { Ok(0) } else { Err(usage.to_string()) };
    }
    let mut status = 0u8;
    for p in paths {
        let path = Path::new(p);
        // symlink_metadata: a symlink to a directory is REMOVED, not followed
        // and recursed into. `rm -rf /sysroot/var/run` runs where that link
        // exists, and following it would empty the real /run.
        let meta = std::fs::symlink_metadata(path);
        let r = match &meta {
            Ok(m) if m.is_dir() && recursive => std::fs::remove_dir_all(path),
            // A directory without `-r` is an ERROR even when empty, as coreutils
            // and busybox both have it. `remove_dir` would quietly succeed and
            // make `rm` and `rmdir` the same command.
            Ok(m) if m.is_dir() => Err(std::io::Error::other(format!("{p} is a directory"))),
            Ok(_) => std::fs::remove_file(path),
            Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
        };
        match r {
            Ok(()) => {}
            // `-f` licenses silence for what is NOT THERE, and nothing else. An
            // EACCES or EBUSY swallowed here aborts a `set -e` boot script with an
            // empty console — the symptom class this landing exists to remove.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !force {
                    crate::emit_err(&format!("rm: {p}: {e}\n"));
                    status = 1;
                }
            }
            Err(e) => {
                crate::emit_err(&format!("rm: {p}: {e}\n"));
                status = 1;
            }
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("td-util-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `ln` makes a SYMLINK or refuses. A hard link is a different object.
    #[test]
    fn ln_requires_the_symbolic_form() {
        let d = scratch("ln");
        let link = d.join("l");
        assert_eq!(ln(&args(&["-s", "/run", &link.to_string_lossy()])), Ok(0));
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("/run"));
        assert!(
            ln(&args(&["/run", &d.join("h").to_string_lossy()])).is_err(),
            "ln without -s must refuse rather than hard-link"
        );
        assert!(ln(&args(&["-s", "/run"])).is_err(), "one operand is a usage error");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `mkdir -p` is idempotent; a bare `mkdir` over an existing dir is not.
    #[test]
    fn mkdir_p_accepts_an_existing_directory() {
        let d = scratch("mkdir");
        let nested = d.join("a/b/c");
        assert_eq!(mkdir(&args(&["-p", &nested.to_string_lossy()])), Ok(0));
        assert!(nested.is_dir());
        assert_eq!(mkdir(&args(&["-p", &nested.to_string_lossy()])), Ok(0), "-p must be idempotent");
        assert_eq!(mkdir(&args(&[&nested.to_string_lossy()])), Ok(1), "without -p it exists");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A non-symlink prints NOTHING and fails.
    ///
    /// Asserting the status alone left the no-output half — the half the docstring
    /// and the boot scripts rely on — unchecked: `[ "$(readlink /home)" = /var/home ]`
    /// would compare EQUAL for a plain directory if this echoed the path back.
    #[test]
    fn readlink_prints_nothing_at_all_on_a_non_symlink() {
        let d = scratch("readlink");
        let f = d.join("f");
        std::fs::write(&f, b"x").unwrap();
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(link_target(&f.to_string_lossy(), &mut out), Ok(1));
        assert!(out.is_empty(), "a non-symlink produced output: {out:?}");

        let mut missing: Vec<u8> = Vec::new();
        assert_eq!(link_target("/nonexistent/td-util", &mut missing), Ok(1));
        assert!(missing.is_empty(), "a missing path produced output");

        let l = d.join("l");
        std::os::unix::fs::symlink("/run", &l).unwrap();
        let mut good: Vec<u8> = Vec::new();
        assert_eq!(link_target(&l.to_string_lossy(), &mut good), Ok(0));
        assert_eq!(good, b"/run\n", "the target was not written verbatim");

        assert!(readlink(&args(&[])).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A DANGLING symlink is removed, not silently left behind.
    ///
    /// This is the case that separates `symlink_metadata` from `metadata`, and the
    /// symlink-to-a-live-directory test below does NOT: `remove_dir_all` already
    /// refuses to follow a top-level symlink, so that one passes either way. With
    /// `metadata` a dangling link reports NotFound, `-f` swallows it, and `rm -rf`
    /// returns 0 having removed nothing.
    #[test]
    fn rm_r_removes_a_dangling_symlink() {
        let d = scratch("rm-dangling");
        let link = d.join("dangling");
        std::os::unix::fs::symlink(d.join("was-never-here"), &link).unwrap();
        assert_eq!(rm(&args(&["-rf", &link.to_string_lossy()])), Ok(0));
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the dangling link survived a successful rm -rf"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An EMPTY directory still needs -r.
    ///
    /// `remove_dir` would quietly succeed here and make `rm` and `rmdir` the same
    /// command; coreutils and busybox both refuse. The -r test below uses a
    /// NON-empty directory, so it cannot see this.
    #[test]
    fn rm_refuses_an_empty_directory_without_r() {
        let d = scratch("rm-empty");
        let sub = d.join("empty");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(rm(&args(&["-f", &sub.to_string_lossy()])), Ok(1));
        assert!(sub.is_dir(), "an empty directory was removed without -r");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `rm -rf` on a symlink-to-directory removes the LINK.
    ///
    /// `/sysroot/var/run` is exactly that symlink, and following it would empty
    /// the real /run — with the boot continuing as though it had tidied up.
    #[test]
    fn rm_r_removes_a_symlink_without_following_it() {
        let d = scratch("rm");
        let real = d.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep"), b"x").unwrap();
        let link = d.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(rm(&args(&["-rf", &link.to_string_lossy()])), Ok(0));
        assert!(!link.exists(), "the link survived");
        assert!(
            real.join("keep").exists(),
            "rm followed the symlink and emptied the directory it pointed at"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `-f` swallows a missing operand; without it, that is status 1.
    #[test]
    fn rm_f_is_silent_about_what_is_not_there() {
        let missing = format!("/nonexistent/td-util-{}", std::process::id());
        assert_eq!(rm(&args(&["-f", &missing])), Ok(0));
        assert_eq!(rm(&args(&[&missing])), Ok(1));
        assert!(rm(&args(&[])).is_err(), "no operand and no -f is a usage error");
        assert_eq!(rm(&args(&["-f"])), Ok(0));
    }

    /// A directory needs -r; without it this must not silently recurse.
    #[test]
    fn rm_refuses_a_directory_without_r() {
        let d = scratch("rmdir");
        let sub = d.join("sub");
        std::fs::create_dir_all(sub.join("inner")).unwrap();
        assert_eq!(rm(&args(&["-f", &sub.to_string_lossy()])), Ok(1), "-f alone must not recurse");
        assert!(sub.is_dir(), "the directory was removed without -r");
        assert_eq!(rm(&args(&["-rf", &sub.to_string_lossy()])), Ok(0));
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
