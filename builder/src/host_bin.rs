//! The host cargo build of a control-plane tool, and the two helpers every
//! child the check loop spawns shares: arming it to die with its parent, and
//! waiting on it under a deadline. Engine-side, since `provision-net` — the
//! verb the evaluator's source warm resolves td-net through, ahead of a
//! recipe check's build — runs the build here (see `engine_set`); the check
//! loop's prelude warm is the other caller. The `net/` sources it builds from
//! are outside the memo key: td-net fetches hash-pinned inputs and decides no
//! verdict.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Resolve `bin` in a `:`-joined PATH fragment (the form `stage0::provision_*`
/// return) to an absolute executable: the child runs under a provisioned
/// toolchain PATH, so the binary must come from THERE, not an ambient lookup.
fn find_in_frags(frags: &str, bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    frags.split(':').filter(|f| !f.is_empty()).find_map(|d| {
        let p = Path::new(d).join(bin);
        // Require the exec bit, matching stage0::find_in_path and
        // gate_bodies::find_in_path_frags: a non-executable same-named file
        // earlier on PATH must not shadow the real tool.
        let ok = std::fs::metadata(&p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        ok.then_some(p)
    })
}

/// td-feed's own completion predicate -- the marker it renames in only once the
/// whole locked closure is published, AND the lock digest that marker carries.
///
/// Presence alone reads both an interrupted warm and a SUPERSEDED one as done.
/// The second is the one that bites quietly: after a dependency bump the marker
/// still sits there, this reader would report the vendor complete, the retry
/// and the report are both suppressed, and the build then fails the vendor
/// gate's set-equality check every run with nothing here saying why.
pub(crate) fn vendor_is_complete(root: &Path, dest: &str, lock: Option<&str>) -> bool {
    let marker = root
        .join(".td-build-cache/crate-vendor")
        .join(dest)
        .join("vendor")
        .join(".warm-complete");
    // Require a regular marker entry and bound the actual read as well.
    let Ok(meta) = std::fs::symlink_metadata(&marker) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 4096 {
        return false;
    }
    use std::io::Read;
    let Ok(file) = std::fs::File::open(&marker) else {
        return false;
    };
    let mut marked = String::new();
    if file.take(4097).read_to_string(&mut marked).is_err() || marked.len() > 4096 {
        return false;
    }
    // No lock named means nothing can vouch for the marker; treat it as cold
    // rather than trust a bare file, which is the fail-open being closed here.
    let Some(lock) = lock else {
        return false;
    };
    let Ok(want) = crate::sha256::sha256_file(&root.join(lock)) else {
        return false;
    };
    marked.lines().next().map(str::trim) == Some(want.as_str())
}

struct NativeVendor(PathBuf);

impl Drop for NativeVendor {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl NativeVendor {
    fn directory(&self) -> PathBuf {
        self.0.join("sources")
    }
}

fn prepare_native_vendor(root: &Path) -> Result<NativeVendor, String> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    if !vendor_is_complete(root, "td-net", Some("net/Cargo.lock")) {
        return Err("native td-net vendor is incomplete or stale; prepare it with the installed td-feed first".into());
    }
    let scratch = root.join(".td-build-cache").join(format!(
        "native-vendor-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&scratch)
        .map_err(|e| format!("create native vendor {}: {e}", scratch.display()))?;
    let prepared = NativeVendor(scratch);
    let lock = std::fs::read_to_string(root.join("net/Cargo.lock"))
        .map_err(|e| format!("read native td-net Cargo.lock: {e}"))?;
    crate::build::validate_cargo_lock_sources(&lock, &[])?;
    let archives = prepared.0.join("archives");
    // td-feed publishes archives. Verify private copies before extraction.
    crate::stage_verified_vendor(
        &root.join(".td-build-cache/crate-vendor/td-net/vendor"),
        &lock,
        &archives,
        false,
    )?;
    let sources = prepared.directory();
    std::fs::create_dir(&sources).map_err(|e| format!("create native Cargo sources: {e}"))?;
    for (index, (name, version, checksum)) in crate::cargo_lock::parse_lock_checksums(&lock)
        .iter()
        .enumerate()
    {
        let nv = format!("{name}-{version}");
        let unpack = prepared.0.join(format!("unpack-{index}"));
        std::fs::create_dir(&unpack).map_err(|e| format!("create crate unpack directory: {e}"))?;
        crate::tar::extract_tar_gz(&archives.join(format!("{nv}.crate")), &unpack)?;
        let package = unpack.join(&nv);
        let entries = std::fs::read_dir(&unpack)
            .map_err(|e| format!("inspect unpacked crate {nv}: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read unpacked crate {nv}: {e}"))?;
        if entries.len() != 1
            || !std::fs::symlink_metadata(&package)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        {
            return Err(format!(
                "crate {nv} must unpack to exactly its own directory"
            ));
        }
        let checksum_path = package.join(".cargo-checksum.json");
        // Never follow an archive-provided checksum symlink.
        match std::fs::remove_file(&checksum_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("replace crate {nv} checksum: {e}")),
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&checksum_path)
            .map_err(|e| format!("create crate {nv} checksum: {e}"))?;
        write!(file, "{{\"files\":{{}},\"package\":\"{checksum}\"}}")
            .map_err(|e| format!("write crate {nv} checksum: {e}"))?;
        std::fs::rename(&package, sources.join(&nv))
            .map_err(|e| format!("publish private crate {nv}: {e}"))?;
    }
    Ok(prepared)
}

/// Build the network preparation helper with the provisioned toolchain.
/// Only net/td-net is supported; both callers pin these arguments.
/// td uses its native GNU target; other hosts use musl and its bundled linker.
/// Both require an empty ELF startup closure before returning a helper. Static
/// GNU linkage does not rule out libc loading NSS modules during name lookup;
/// these helpers run on the build host, with its matching libc runtime.
///
/// Explicit --target keeps static target flags off host build scripts and proc
/// macros. Pin the compiler, C tools, wrappers, linker and highest-precedence
/// encoded flags so ambient Cargo settings cannot replace the selected tools.
/// Every cc-rs target spelling is pinned because target-specific settings take
/// precedence over plain CC/AR. Failure is reported before the caller's existing
/// fallback; a dynamically linked helper is never returned.
pub(crate) fn host_cargo_bin(
    root: &Path,
    dir: &str,
    bin: &str,
    deadline: Option<Instant>,
) -> Option<PathBuf> {
    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let rustpath = match crate::stage0::provision_rust(&penv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "td-builder check: static {bin}: no rust toolchain ({e}) — skipping host build"
            );
            return None;
        }
    };
    let ccpath = match crate::stage0::provision_cc(&penv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: no C toolchain ({e}) — skipping host build");
            return None;
        }
    };
    let Some(cargo) = find_in_frags(&rustpath, "cargo") else {
        eprintln!("td-builder check: static {bin}: no cargo on the provisioned rust toolchain — skipping host build");
        return None;
    };
    let Some(rustc) = find_in_frags(&rustpath, "rustc") else {
        eprintln!("td-builder check: static {bin}: no rustc on the provisioned rust toolchain — skipping host build");
        return None;
    };
    let Some(cc) = find_in_frags(&ccpath, "cc").or_else(|| find_in_frags(&ccpath, "gcc")) else {
        eprintln!("td-builder check: static {bin}: no cc/gcc on the provisioned C toolchain — skipping host build");
        return None;
    };
    let Some(ar) = find_in_frags(&ccpath, "ar") else {
        eprintln!("td-builder check: static {bin}: no ar on the provisioned C toolchain — skipping host build");
        return None;
    };

    // Host triple (`rustc -vV`'s `host:` line): the triple cargo compiles the host
    // build scripts / proc-macros for — those link with the provisioned cc.
    let host_triple = match crate::stage0::rustc_host_triple(&rustc) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: {e} — skipping host build");
            return None;
        }
    };
    let target = crate::stage0::control_plane_target(&penv);
    // Target artifacts get static flags; host build scripts use the C linker.
    let host_linker_var = crate::stage0::target_linker_var(&host_triple);
    let encoded_rustflags = match crate::stage0::control_plane_flags(&penv, &cc) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("td-builder check: static {bin}: {error} — skipping host build");
            return None;
        }
    };
    // Pin every cc-rs spelling, including target-specific overrides.
    let target_us = target.replace('-', "_");
    let cc_target = format!("CC_{target}");
    let cc_target_us = format!("CC_{target_us}");
    let ar_target = format!("AR_{target}");
    let ar_target_us = format!("AR_{target_us}");
    let ambient_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{rustpath}:{ccpath}:{ambient_path}");

    let native_vendor = if penv.native_td {
        match prepare_native_vendor(root) {
            Ok(prepared) => Some(prepared),
            Err(error) => {
                eprintln!("td-builder check: static {bin}: {error}");
                return None;
            }
        }
    } else {
        None
    };
    let mut command = Command::new(&cargo);
    command
        .args(["build", "--release", "--quiet", "--target", target])
        .current_dir(root.join(dir))
        .env("PATH", &new_path)
        // Pin the compiler itself: an inherited RUSTC would build with a
        // different rustc than the one we read the triple from, and an inherited
        // RUSTC_WRAPPER (e.g. sccache) would interpose on the control-plane build.
        // Set the wrappers to "" (not env_remove): an ABSENT var lets cargo fall
        // back to a `.cargo/config.toml` `build.rustc-wrapper`, whereas an empty
        // value means "no wrapper" regardless of config (Agy review, PR #534).
        .env("RUSTC", &rustc)
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CC", &cc)
        .env("HOST_CC", &cc)
        .env("TARGET_CC", &cc)
        .env(&cc_target, &cc)
        .env(&cc_target_us, &cc)
        .env("AR", &ar)
        .env("HOST_AR", &ar)
        .env("TARGET_AR", &ar)
        .env(&ar_target, &ar)
        .env(&ar_target_us, &ar)
        .env(&host_linker_var, &cc)
        .env("CARGO_ENCODED_RUSTFLAGS", &encoded_rustflags)
        .stdin(Stdio::null());
    if let Some(prepared) = &native_vendor {
        let path = match prepared.directory().canonicalize() {
            Ok(path) => path,
            Err(e) => {
                eprintln!("td-builder check: static {bin}: resolve native vendor: {e}");
                return None;
            }
        };
        let Some(path) = path.to_str() else {
            eprintln!("td-builder check: static {bin}: native vendor path is not UTF-8");
            return None;
        };
        let directory = td_engine::json::Json::Str(path.to_string()).to_json_string();
        command.args([
            "--offline",
            "--frozen",
            "--config",
            "source.crates-io.replace-with=\"td-vendor\"",
            "--config",
            &format!("source.td-vendor.directory={directory}"),
        ]);
    }
    arm_check_child(&mut command);
    let child = command.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "td-builder check: static {bin}: cannot spawn cargo ({e}) — skipping host build"
            );
            return None;
        }
    };
    if !wait_with_deadline(&mut child, deadline) {
        return None;
    }
    let p = root
        .join(dir)
        .join("target")
        .join(target)
        .join("release")
        .join(bin);
    if !p.is_file() {
        return None;
    }
    // Fail closed on a non-static result rather than hand a dynamic control-plane
    // binary to the warm (re #469).
    if let Err(e) = crate::elf::assert_static(&p) {
        eprintln!(
            "td-builder check: static {bin}: host build produced a non-static binary ({e}) — skipping"
        );
        return None;
    }
    Some(p)
}

/// The host td-net multicall itself (no applet link): the `provision-net` verb's
/// resolver, so a caller outside this binary — the evaluator's interactive source
/// warm — gets the SAME statically linked build the prelude uses rather than a
/// second copy of it. Applet dispatch is by argv there (`td-net feed …`), which is
/// why this returns the multicall and `host_net_applet` returns a link.
pub(crate) fn host_td_net(root: &Path) -> Option<PathBuf> {
    host_cargo_bin(root, "net", "td-net", None)
}

/// Wait for a warm child under an optional deadline: block when there is
/// none; past it (or on a wait error), kill the child and report failure —
/// a killed child is a failed warm step, never a failed check.
pub(crate) fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Option<Instant>,
) -> bool {
    let Some(d) = deadline else {
        return child.wait().map(|st| st.success()).unwrap_or(false);
    };
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) if Instant::now() >= d => {
                let _ = crate::sys::kill_child_recorded(
                    child,
                    "the warm step outlived its deadline (check-loop warm)",
                );
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                let _ = crate::sys::kill_child_recorded(
                    child,
                    &format!("waiting for the warm step failed: {e} (check-loop warm)"),
                );
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub(crate) fn arm_check_child(cmd: &mut Command) {
    // Every child spawned before the final gate sandbox must die when this
    // hosted runner dies. PR_SET_PDEATHSIG is reset across fork, so arming only
    // the runner in check_host is not enough for provisioning and warm tools.
    crate::sandbox::die_with_parent(cmd);
}

#[cfg(test)]
mod native_vendor_tests {
    use super::*;
    use std::fs;

    fn archive() -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, data) in [
            (
                "tinydep-0.1.0/Cargo.toml",
                "[package]\nname = \"tinydep\"\nversion = \"0.1.0\"\n",
            ),
            ("tinydep-0.1.0/src/lib.rs", "pub fn value() -> u8 { 7 }\n"),
        ] {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            tar.extend_from_slice(&header);
            tar.extend_from_slice(data.as_bytes());
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.resize(tar.len() + 1024, 0);
        let len = u16::try_from(tar.len()).unwrap();
        let mut gzip = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255, 1];
        gzip.extend_from_slice(&len.to_le_bytes());
        gzip.extend_from_slice(&(!len).to_le_bytes());
        gzip.extend_from_slice(&tar);
        gzip.extend_from_slice(&crate::crc32::crc32(&tar).to_le_bytes());
        gzip.extend_from_slice(&(tar.len() as u32).to_le_bytes());
        gzip
    }

    fn fixture(tag: &str) -> NativeVendor {
        let root =
            std::env::temp_dir().join(format!("td-native-vendor-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let fixture = NativeVendor(root);
        let vendor = fixture.0.join(".td-build-cache/crate-vendor/td-net/vendor");
        fs::create_dir_all(&vendor).unwrap();
        fs::create_dir_all(fixture.0.join("net/src")).unwrap();
        fs::write(vendor.join("tinydep-0.1.0.crate"), archive()).unwrap();
        let checksum = crate::sha256::sha256_file(&vendor.join("tinydep-0.1.0.crate")).unwrap();
        fs::write(fixture.0.join("net/Cargo.toml"), "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\n[dependencies]\ntinydep = \"=0.1.0\"\n[workspace]\n").unwrap();
        fs::write(
            fixture.0.join("net/src/lib.rs"),
            "pub fn value() -> u8 { tinydep::value() }\n",
        )
        .unwrap();
        let lock = format!("version = 4\n\n[[package]]\nname = \"consumer\"\nversion = \"0.1.0\"\ndependencies = [\"tinydep\"]\n\n[[package]]\nname = \"tinydep\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n");
        fs::write(fixture.0.join("net/Cargo.lock"), lock).unwrap();
        let lock_digest = crate::sha256::sha256_file(&fixture.0.join("net/Cargo.lock")).unwrap();
        fs::write(vendor.join(".warm-complete"), format!("{lock_digest}\n1\n")).unwrap();
        fixture
    }

    #[test]
    fn prepared_archives_resolve_in_cargo_offline_and_cleanup() {
        let fixture = fixture("resolve");
        let prepared = prepare_native_vendor(&fixture.0).unwrap();
        let path = prepared.directory();
        assert!(path.join("tinydep-0.1.0/src/lib.rs").is_file());
        let cargo_home = fixture.0.join("cargo-home");
        fs::create_dir(&cargo_home).unwrap();
        let directory = td_engine::json::Json::Str(path.to_str().unwrap().into()).to_json_string();
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args([
                "metadata",
                "--offline",
                "--frozen",
                "--format-version=1",
                "--config",
                "source.crates-io.replace-with=\"td-vendor\"",
                "--config",
                &format!("source.td-vendor.directory={directory}"),
            ])
            .current_dir(fixture.0.join("net"))
            .env("CARGO_HOME", cargo_home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("tinydep"));
        drop(prepared);
        assert!(!path.exists());
    }

    #[test]
    fn prepared_archives_refuse_missing_extra_and_tampered_inputs() {
        let fixture = fixture("refusals");
        let vendor = fixture.0.join(".td-build-cache/crate-vendor/td-net/vendor");
        let archive_path = vendor.join("tinydep-0.1.0.crate");
        fs::write(&archive_path, b"tampered").unwrap();
        assert!(prepare_native_vendor(&fixture.0)
            .err()
            .unwrap()
            .contains("committed-lock"));
        fs::remove_file(&archive_path).unwrap();
        assert!(prepare_native_vendor(&fixture.0).is_err());
        fs::write(&archive_path, archive()).unwrap();
        fs::write(vendor.join("extra-0.1.0.crate"), archive()).unwrap();
        assert!(prepare_native_vendor(&fixture.0).is_err());
        assert!(fs::read_dir(fixture.0.join(".td-build-cache"))
            .unwrap()
            .all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("native-vendor-")));
    }
}
