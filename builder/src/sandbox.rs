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

    // The build dir is `guix-build-<drvName>-0`. For a store-path drv that is
    // storePathToName(drvPath). For an emitted `.drv` handed in from outside the
    // store (td-drv-build builds the file td WROTE), derive the same name from the
    // first output's store name + ".drv" (drvName == outName + ".drv" for these
    // single-output subjects). Store-path inputs (the td-builder rung) are
    // unaffected — the first branch still wins.
    let drv_name = match store_path_name(drv_path) {
        Ok(n) => n.to_string(),
        Err(_) => {
            let out0 = drv
                .outputs
                .first()
                .ok_or_else(|| err("derivation has no outputs".into()))?;
            format!("{}.drv", store_path_name(&out0.path)?)
        }
    };
    let build_dir = format!("/tmp/guix-build-{}-0", drv_name);
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

/// A host path to expose inside the loop sandbox (rbind-mounted at the same
/// path in the new root). `readonly` remounts it read-only after binding.
pub struct Bind {
    pub src: String,
    pub readonly: bool,
}

/// The loop-sandbox DEV-SHELL (vs. the build jail above): pivot into a fresh
/// tmpfs root that exposes ONLY `binds` (rbind, the same path inside) plus a
/// writable tmpfs at each of `tmpfs_dirs`; the host filesystem is otherwise
/// gone. `path_prepend` (the host-guix bin dir, itself under the exposed
/// `/gnu/store`) leads PATH; `home` is HOME (must be one of `tmpfs_dirs`, i.e.
/// writable). Runs `cmd args` and returns its exit status. NEWUSER|NEWNS only
/// — uid/gid are IDENTITY-mapped so the host daemon's peer-cred check still
/// sees the real host uid; network-namespace parity with `guix shell -C` is a
/// deferred follow-up (this runs inside check.sh's offline outer container).
pub fn host_shell(
    cmd: &str,
    args: &[String],
    binds: &[Bind],
    tmpfs_dirs: &[String],
    path_prepend: &str,
    home: &str,
    scratch: &Path,
) -> io::Result<std::process::ExitStatus> {
    let newroot = scratch.join("root");
    fs::create_dir_all(&newroot)?;
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    // Precompute every CString in the parent (the child's pre_exec only does
    // syscalls + fs ops, mirroring `build` above).
    let newroot_c = CString::new(newroot.as_os_str().as_encoded_bytes()).unwrap();
    let root_c = CString::new("/").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    let oldroot_rel = newroot.join("oldroot");
    let oldroot_rel_c = CString::new(oldroot_rel.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_abs_c = CString::new("/oldroot").unwrap();

    // (src_c, target_dir, target_c, readonly) for each bind.
    let mut bind_specs: Vec<(CString, PathBuf, CString, bool)> = Vec::with_capacity(binds.len());
    for b in binds {
        let target = newroot.join(b.src.strip_prefix('/').unwrap_or(&b.src));
        bind_specs.push((
            CString::new(b.src.as_str()).map_err(|_| err(format!("{}: NUL in path", b.src)))?,
            target.clone(),
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
            b.readonly,
        ));
    }
    // (target_dir, target_c) for each writable tmpfs mount.
    let mut tmpfs_specs: Vec<(PathBuf, CString)> = Vec::with_capacity(tmpfs_dirs.len());
    for d in tmpfs_dirs {
        let target = newroot.join(d.strip_prefix('/').unwrap_or(d));
        tmpfs_specs.push((
            target.clone(),
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
        ));
    }

    let path_env = if path_prepend.is_empty() {
        "/run/current-system/profile/bin:/run/current-system/profile/sbin".to_string()
    } else {
        format!("{path_prepend}:/run/current-system/profile/bin")
    };

    let mut command = Command::new(cmd);
    command.args(args);
    command.env_clear();
    command.env("PATH", &path_env);
    command.env("HOME", home);
    command.env("TMPDIR", "/tmp");
    // guix reads these for locale + terminal; harmless, keeps output identical
    // to the outer shell.
    if let Ok(v) = std::env::var("TERM") {
        command.env("TERM", v);
    }
    if let Ok(v) = std::env::var("GUIX_LOCPATH") {
        command.env("GUIX_LOCPATH", v);
    }

    unsafe {
        command.pre_exec(move || {
            sys::unshare(
                sys::CLONE_NEWUSER | sys::CLONE_NEWNS | sys::CLONE_NEWIPC | sys::CLONE_NEWUTS,
            )?;
            fs::write("/proc/self/setgroups", "deny")?;
            // Standard rootless map: inside-root (uid 0) → the host uid, so the
            // process owns the fresh tmpfs root/dirs (writable) while the host
            // daemon's SO_PEERCRED still resolves to the real host user (the
            // kernel translates inner uid 0 back to the host uid).
            fs::write("/proc/self/uid_map", format!("0 {host_uid} 1"))?;
            fs::write("/proc/self/gid_map", format!("0 {host_gid} 1"))?;
            // Everything below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE)?;
            // A fresh tmpfs is the new root (also makes it a mount point, which
            // pivot_root requires).
            sys::mount(Some(&tmpfs_c), &newroot_c, Some(&tmpfs_c), 0)?;
            // Expose each requested host path (rbind), read-only where asked.
            for (src_c, target_dir, target_c, readonly) in &bind_specs {
                fs::create_dir_all(target_dir)?;
                sys::mount(Some(src_c), target_c, None, sys::MS_BIND | sys::MS_REC)?;
                if *readonly {
                    sys::mount(
                        None,
                        target_c,
                        None,
                        sys::MS_REMOUNT | sys::MS_BIND | sys::MS_REC | sys::MS_RDONLY,
                    )?;
                }
            }
            // Writable scratch tmpfs mounts (/tmp, HOME).
            for (target_dir, target_c) in &tmpfs_specs {
                fs::create_dir_all(target_dir)?;
                sys::mount(Some(&tmpfs_c), target_c, Some(&tmpfs_c), 0)?;
            }
            // pivot into the new root and drop the old one entirely.
            fs::create_dir_all(&oldroot_rel)?;
            sys::pivot_root(&newroot_c, &oldroot_rel_c)?;
            std::env::set_current_dir("/")?;
            sys::umount2(&oldroot_abs_c, sys::MNT_DETACH)?;
            let _ = fs::remove_dir("/oldroot");
            Ok(())
        });
    }

    command
        .status()
        .map_err(|e| err(format!("spawning {cmd} in host-sandbox: {e}")))
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
