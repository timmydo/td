//! The S3 build sandbox: execute a parsed `.drv` in a fresh user namespace,
//! replicating the pinned daemon's guest-visible contract (read off
//! nix/libstore/build.cc and recorded in plan/td-builder.md Q4):
//!   - namespaces: NEWUSER|NEWNS|NEWNET|NEWIPC|NEWUTS (the immediate-effect
//!     set; NEWNET makes the build offline by construction — NEWPID/proc and
//!     the full chroot layout are S4 parity work);
//!   - uid/gid: guest 30001/30000 mapped over the invoking user, setgroups
//!     denied (build.cc defaultGuestUID/GID, initializeUserNamespace);
//!   - store: every closure item bind-mounted into a staged directory which
//!     is then rbind-mounted over /gnu/store, so the builder sees real store
//!     paths while writes land in the scratch directory (the rootless rung's
//!     mechanics) and the bound inputs stay protected by their host-root
//!     ownership;
//!   - build dir: a fresh tmpfs /tmp with /tmp/guix-build-<drvname>-0 (0700,
//!     <drvname> keeps the .drv suffix), cwd there;
//!   - env: cleared, then PATH/HOME/NIX_STORE/NIX_BUILD_CORES, the drv's
//!     env, then NIX_BUILD_TOP/TMPDIR/TEMPDIR/TMP/TEMP/PWD — build.cc's
//!     exact set and override order (the TMPDIR group wins over drv env).

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::drv::Derivation;
use crate::sys;

const STORE: &str = "/gnu/store/";
const GUEST_UID: u32 = 30001;
const GUEST_GID: u32 = 30000;

fn err(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, what)
}

/// build.cc storePathToName: strip the store dir and the 32-char base32
/// hash + dash. For a drv path the result KEEPS the .drv suffix.
pub fn store_path_name(path: &str) -> io::Result<&str> {
    let base = path
        .strip_prefix(STORE)
        .ok_or_else(|| err(format!("{path}: not a store path")))?;
    if base.len() > 33 && base.as_bytes()[32] == b'-' && !base.contains('/') {
        Ok(&base[33..])
    } else {
        Err(err(format!("{path}: malformed store path basename")))
    }
}

/// Run the drv's builder inside the namespace sandbox. `closure` lists every
/// store path the build may see (the staged store's contents); `scratch` is
/// a writable host directory. On success returns (output name, host-side
/// path under scratch/newstore) for every drv output, each verified to
/// exist.
pub fn build(
    drv: &Derivation,
    drv_path: &str,
    closure: &[String],
    scratch: &Path,
) -> io::Result<Vec<(String, PathBuf)>> {
    if drv.platform != "x86_64-linux" {
        return Err(err(format!(
            "platform `{}' is not x86_64-linux — refusing to build",
            drv.platform
        )));
    }

    // Stage the bind targets in the parent (plain file ops on our scratch);
    // the mounts themselves happen in the child's namespace.
    let newstore = scratch.join("newstore");
    fs::create_dir_all(&newstore)?;
    let mut binds: Vec<(CString, CString)> = Vec::with_capacity(closure.len());
    for p in closure {
        let meta = fs::symlink_metadata(p)
            .map_err(|e| err(format!("closure item {p}: {e}")))?;
        let target = newstore.join(
            p.strip_prefix(STORE)
                .ok_or_else(|| err(format!("closure item {p}: not a store path")))?,
        );
        if meta.is_dir() {
            fs::create_dir_all(&target)?;
        } else if meta.is_file() {
            fs::File::create(&target)?;
        } else {
            // A symlink cannot be bind-mounted; no pinned-channel closure
            // has top-level symlink store items — refuse rather than guess.
            return Err(err(format!("closure item {p}: unsupported file type")));
        }
        binds.push((
            CString::new(p.as_str()).map_err(|_| err(format!("{p}: NUL in path")))?,
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
        ));
    }

    let build_dir = format!("/tmp/guix-build-{}-0", store_path_name(drv_path)?);
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    let newstore_c = CString::new(newstore.as_os_str().as_encoded_bytes()).unwrap();
    let root_c = CString::new("/").unwrap();
    let store_c = CString::new("/gnu/store").unwrap();
    let tmp_c = CString::new("/tmp").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    let build_dir_owned = build_dir.clone();

    let mut cmd = Command::new(&drv.builder);
    cmd.args(&drv.args);
    cmd.env_clear();
    // build.cc's exact assembly order; Command's env map gives the same
    // override semantics (later set wins).
    cmd.env("PATH", "/path-not-set");
    cmd.env("HOME", "/homeless-shelter");
    cmd.env("NIX_STORE", "/gnu/store");
    cmd.env("NIX_BUILD_CORES", "1");
    for (k, v) in &drv.env {
        cmd.env(k, v);
    }
    for k in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        cmd.env(k, &build_dir);
    }

    unsafe {
        cmd.pre_exec(move || {
            sys::unshare(
                sys::CLONE_NEWUSER
                    | sys::CLONE_NEWNS
                    | sys::CLONE_NEWNET
                    | sys::CLONE_NEWIPC
                    | sys::CLONE_NEWUTS,
            )?;
            // Map the guest ids before touching anything else so file
            // creation below happens as 30001/30000, not the overflow id.
            fs::write("/proc/self/setgroups", "deny")?;
            fs::write("/proc/self/uid_map", format!("{GUEST_UID} {host_uid} 1"))?;
            fs::write("/proc/self/gid_map", format!("{GUEST_GID} {host_gid} 1"))?;
            // Keep every mount below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE)?;
            for (src, dst) in &binds {
                sys::mount(Some(src), dst, None, sys::MS_BIND)?;
            }
            sys::mount(Some(&newstore_c), &store_c, None, sys::MS_BIND | sys::MS_REC)?;
            sys::mount(Some(&tmpfs_c), &tmp_c, Some(&tmpfs_c), 0)?;
            fs::DirBuilder::new().mode(0o700).create(&build_dir_owned)?;
            std::env::set_current_dir(&build_dir_owned)?;
            Ok(())
        });
    }

    let status = cmd
        .status()
        .map_err(|e| err(format!("spawning builder {}: {e}", drv.builder)))?;
    if !status.success() {
        return Err(err(format!(
            "builder for {drv_path} failed: {status}"
        )));
    }

    let mut outputs = Vec::with_capacity(drv.outputs.len());
    for o in &drv.outputs {
        let host = newstore.join(
            o.path
                .strip_prefix(STORE)
                .ok_or_else(|| err(format!("output {}: not a store path", o.path)))?,
        );
        fs::symlink_metadata(&host).map_err(|_| {
            err(format!(
                "builder exited 0 but did not produce output `{}' ({})",
                o.name, o.path
            ))
        })?;
        outputs.push((o.name.clone(), host));
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_name_strips_hash() {
        assert_eq!(
            store_path_name("/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-0.1.0.drv")
                .unwrap(),
            "td-builder-0.1.0.drv"
        );
        assert!(store_path_name("/tmp/x").is_err());
        assert!(store_path_name("/gnu/store/tooshort-x").is_err());
        // A slash after the hash means a path INSIDE an item, not an item.
        assert!(store_path_name(
            "/gnu/store/xiwgysq1h8dd2k5mkb94ky8vrgcp10dz-td-builder-0.1.0/bin/td-builder"
        )
        .is_err());
    }
}
