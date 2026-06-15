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
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)?;
            for (src, dst) in &binds {
                sys::mount(Some(src), dst, None, sys::MS_BIND, None)?;
            }
            sys::mount(Some(&newstore_c), &store_c, None, sys::MS_BIND | sys::MS_REC, None)?;
            sys::mount(Some(&tmpfs_c), &tmp_c, Some(&tmpfs_c), 0, None)?;
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
    /// When `readonly`, tolerate a FAILED read-only remount (keep the bind
    /// writable) instead of erroring. Set ONLY for defense-in-depth ro binds the
    /// kernel may forbid remounting in a child user namespace — e.g.
    /// `/sys/fs/cgroup` (cgroup2, owned by the host userns: a child userns lacks
    /// CAP_SYS_ADMIN over it, so MS_REMOUNT|MS_RDONLY → EPERM on some kernels,
    /// e.g. GitHub's azure runner). NEVER for binds whose read-only is
    /// load-bearing (the store): those still error on a failed remount.
    pub ro_optional: bool,
}

/// The loop-sandbox DEV-SHELL (vs. the build jail above): pivot into a fresh
/// tmpfs root that exposes ONLY `binds` (rbind, the same path inside) plus a
/// writable tmpfs at each of `tmpfs_dirs`; the host filesystem is otherwise
/// gone. `path_env` is the full PATH (empty → a default); `home` is HOME;
/// `workdir` is the cwd to enter after pivot (empty → `/`); `extra_env` is
/// caller-preserved env (e.g. the check-memo `TD_CHECK_*`). Runs `cmd args` and
/// returns its exit status. Unshares
/// NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS and runs the command as PID 1 of the
/// new PID namespace with a private /proc mounted by that PID-1 process — full
/// `guix shell -C` parity, so nested containers (the loop-sandbox/loop-rung
/// equivalence oracle, the rootless rung) can create their own PID ns + /proc.
/// uid/gid use the IDENTITY map (host uid → itself) so the host daemon's
/// peer-cred check still sees the real host uid, and its own network namespace
/// (loopback brought up) matches `guix shell -C`'s offline posture — the daemon
/// stays reachable over the Unix socket on the bound /var/guix.
#[allow(clippy::too_many_arguments)]
pub fn host_shell(
    cmd: &str,
    args: &[String],
    binds: &[Bind],
    tmpfs_dirs: &[String],
    path_env: &str,
    home: &str,
    workdir: &str,
    extra_env: &[(String, String)],
    scratch: &Path,
) -> io::Result<std::process::ExitStatus> {
    let newroot = scratch.join("root");
    fs::create_dir_all(&newroot)?;
    let host_uid = sys::getuid();
    let host_gid = sys::getgid();

    // Precompute every CString in the parent (the child's pre_exec only does
    // syscalls + fs ops, mirroring `build` above).
    // tmpfs root/dirs are owned by uid 0 of the new userns by default; with the
    // identity uid map (below) that is unmapped, so set the owner explicitly to
    // the host uid/gid via the tmpfs `uid=/gid=` mount data — keeps the dirs
    // writable while the process stays the (non-root) host uid.
    let tmpfs_data = CString::new(format!("uid={host_uid},gid={host_gid}")).unwrap();
    let newroot_c = CString::new(newroot.as_os_str().as_encoded_bytes()).unwrap();
    let root_c = CString::new("/").unwrap();
    let tmpfs_c = CString::new("tmpfs").unwrap();
    // A FRESH procfs is mounted at <newroot>/proc by the PID-1 child (below), so
    // /proc reflects the sandbox's OWN PID namespace, not the host's. The host
    // /proc is no longer bound in (main.rs drops it from the exposure set).
    let proc_c = CString::new("proc").unwrap();
    let proc_target_dir = newroot.join("proc");
    let proc_target_c = CString::new(proc_target_dir.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_rel = newroot.join("oldroot");
    let oldroot_rel_c = CString::new(oldroot_rel.as_os_str().as_encoded_bytes()).unwrap();
    let oldroot_abs_c = CString::new("/oldroot").unwrap();

    // (src_c, target_dir, target_c, readonly, ro_optional) for each bind.
    let mut bind_specs: Vec<(CString, PathBuf, CString, bool, bool)> =
        Vec::with_capacity(binds.len());
    for b in binds {
        let target = newroot.join(b.src.strip_prefix('/').unwrap_or(&b.src));
        bind_specs.push((
            CString::new(b.src.as_str()).map_err(|_| err(format!("{}: NUL in path", b.src)))?,
            target.clone(),
            CString::new(target.as_os_str().as_encoded_bytes())
                .map_err(|_| err(format!("{}: NUL in path", target.display())))?,
            b.readonly,
            b.ro_optional,
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

    let path_env = if path_env.is_empty() {
        "/run/current-system/profile/bin:/run/current-system/profile/sbin"
    } else {
        path_env
    };
    let workdir = if workdir.is_empty() { "/" } else { workdir };
    let workdir_owned = workdir.to_string();

    let mut command = Command::new(cmd);
    command.args(args);
    command.env_clear();
    command.env("PATH", path_env);
    command.env("HOME", home);
    command.env("TMPDIR", "/tmp");
    // Caller-preserved env (e.g. the check-memo TD_CHECK_* identity).
    for (k, v) in extra_env {
        command.env(k, v);
    }
    // guix reads these: TERM/GUIX_LOCPATH for terminal+locale; USER/LOGNAME so it
    // finds the per-user profile (/var/guix/profiles/per-user/$USER) — without
    // them `guix time-machine` falls back to the root-owned default profile and
    // EPERMs; GUIX_BUILD_OPTIONS carries the loop's --no-substitutes/--no-offload
    // posture (check.sh sets it for the in-loop `guix build`/`system` calls).
    // Harmless, and keeps behaviour identical to the outer shell.
    for k in [
        "TERM",
        "GUIX_LOCPATH",
        "USER",
        "LOGNAME",
        "GUIX_BUILD_OPTIONS",
        "GUIX_ENVIRONMENT",
    ] {
        if let Ok(v) = std::env::var(k) {
            command.env(k, v);
        }
    }

    unsafe {
        command.pre_exec(move || {
            // New USER + PID + mount + net + IPC + UTS namespaces. NEWPID is in
            // the SAME unshare as NEWUSER so the new PID namespace is OWNED by the
            // new user namespace (the kernel applies NEWUSER first); the fork
            // below then lands the command at PID 1 of that PID namespace, where a
            // private /proc reflects it — full parity with `guix shell -C`, so
            // nested containers (the loop-sandbox/loop-rung equivalence oracle and
            // the rootless rung) can create their own PID ns + /proc instead of
            // tripping over the host's root-owned PID 1.
            sys::unshare(
                sys::CLONE_NEWUSER
                    | sys::CLONE_NEWNS
                    | sys::CLONE_NEWPID
                    | sys::CLONE_NEWNET
                    | sys::CLONE_NEWIPC
                    | sys::CLONE_NEWUTS,
            )
            .map_err(|e| {
                sys::warn(b"td-builder host-sandbox: FAILED at unshare(NEWUSER|NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS)\n");
                e
            })?;
            fs::write("/proc/self/setgroups", "deny")
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED writing /proc/self/setgroups\n"); e })?;
            // IDENTITY map (host uid/gid → itself), exactly like `guix shell -C`:
            // the process stays the NON-root host uid inside, so file-permission
            // checks (e.g. sqlite's access(W_OK) on the root-owned store DB)
            // behave as on the host — a uid-0 map would make root bypass them and
            // then fail on the real write. tmpfs ownership is handled via the
            // `uid=/gid=` mount data instead. The daemon's SO_PEERCRED sees the
            // real host uid either way.
            fs::write("/proc/self/uid_map", format!("{host_uid} {host_uid} 1"))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED writing /proc/self/uid_map\n"); e })?;
            fs::write("/proc/self/gid_map", format!("{host_gid} {host_gid} 1"))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED writing /proc/self/gid_map\n"); e })?;
            // Own network namespace (offline by construction, like `guix shell
            // -C`); bring its loopback up to match that posture. The daemon
            // socket is a Unix socket on the bound /var/guix, so it stays
            // reachable across the netns boundary.
            sys::bring_loopback_up()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED bringing loopback up\n"); e })?;
            // Fork: the child is PID 1 of the new PID namespace and goes on to set
            // up the mounts + exec the command; THIS process (the PID-ns parent,
            // still in the outer PID ns) only waits for it and propagates its exit
            // via exit_group. It must NOT fall through to std's exec path — the
            // command is exec'd exactly once, as PID 1. Stdio is inherited
            // directly, so output still streams; only the exit status flows here.
            let pid = sys::fork()
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at fork\n"); e })?;
            if pid != 0 {
                let status = sys::waitpid(pid)?;
                let code = if status & 0x7f == 0 {
                    (status >> 8) & 0xff
                } else {
                    128 + (status & 0x7f)
                };
                sys::exit_group(code);
            }
            // --- PID 1 of the new PID namespace, from here on ---
            // Everything below private to this namespace.
            sys::mount(None, &root_c, None, sys::MS_REC | sys::MS_PRIVATE, None)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at mount(/, MS_REC|MS_PRIVATE)\n"); e })?;
            // A fresh tmpfs is the new root (also makes it a mount point, which
            // pivot_root requires), owned by the host uid/gid.
            sys::mount(Some(&tmpfs_c), &newroot_c, Some(&tmpfs_c), 0, Some(&tmpfs_data))
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting the tmpfs root\n"); e })?;
            // Expose each requested host path (rbind), read-only where asked.
            for (src_c, target_dir, target_c, readonly, ro_optional) in &bind_specs {
                fs::create_dir_all(target_dir)?;
                sys::mount(Some(src_c), target_c, None, sys::MS_BIND | sys::MS_REC, None)
                    .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED bind-mounting an exposed path\n"); e })?;
                if *readonly {
                    let ro = sys::mount(
                        None,
                        target_c,
                        None,
                        sys::MS_REMOUNT | sys::MS_BIND | sys::MS_REC | sys::MS_RDONLY,
                        None,
                    );
                    // A child userns cannot remount-ro a mount owned by the host
                    // userns (e.g. /sys/fs/cgroup → EPERM on the azure runner).
                    // For ro_optional binds that ro is defense-in-depth (crun runs
                    // --cgroup-manager=disabled and never writes cgroup), so keep
                    // the bind writable rather than fail the whole sandbox. For
                    // every other ro bind (the store) the read-only is load-bearing
                    // — a failed remount is fatal.
                    if let Err(e) = ro {
                        if *ro_optional {
                            sys::warn(b"td-builder host-sandbox: ro-remount not permitted for an ro_optional bind; kept writable (best-effort)\n");
                        } else {
                            sys::warn(b"td-builder host-sandbox: FAILED ro-remounting an exposed path\n");
                            return Err(e);
                        }
                    }
                }
            }
            // A FRESH procfs reflecting THIS PID namespace (we are its PID 1) —
            // NOT the host /proc. Nested containers write /proc/<pid>/setgroups
            // and friends against this; the host /proc (root-owned PID 1) refused
            // those writes from the non-root sandbox uid.
            fs::create_dir_all(&proc_target_dir)?;
            sys::mount(Some(&proc_c), &proc_target_c, Some(&proc_c), 0, None)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting a fresh /proc\n"); e })?;
            // Writable scratch tmpfs mounts (/tmp, HOME), owned by the host uid.
            for (target_dir, target_c) in &tmpfs_specs {
                fs::create_dir_all(target_dir)?;
                sys::mount(Some(&tmpfs_c), target_c, Some(&tmpfs_c), 0, Some(&tmpfs_data))
                    .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED mounting a scratch tmpfs\n"); e })?;
            }
            // pivot into the new root and drop the old one entirely.
            fs::create_dir_all(&oldroot_rel)?;
            sys::pivot_root(&newroot_c, &oldroot_rel_c)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED at pivot_root\n"); e })?;
            std::env::set_current_dir("/")?;
            sys::umount2(&oldroot_abs_c, sys::MNT_DETACH)
                .map_err(|e| { sys::warn(b"td-builder host-sandbox: FAILED unmounting oldroot\n"); e })?;
            let _ = fs::remove_dir("/oldroot");
            // Enter the requested working directory (e.g. the exposed worktree).
            std::env::set_current_dir(&workdir_owned)?;
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
