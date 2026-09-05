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

/// Build a td network tool (`dir` = `net`, the merged td-net multicall — the only
/// crate this is called for, via `check_loop::host_net_applet`) with the HOST cargo, STATICALLY
/// linked (crt-static against a matched
/// glibc), and return the binary. Best-effort: any missing piece (no toolchain,
/// no static glibc, a non-static result) logs and returns `None` so the warm
/// degrades to the td-built binary or is skipped — it never returns a
/// DYNAMICALLY linked control-plane tool, which would drag a mutable
/// host/guix-home runpath and flake with `libgcc_s.so.1` exit 127 (re #469).
///
/// Scope: td-feed/td-fetch run on the HOST (the warm's network prep), where NSS
/// is present, so the static glibc's runtime `dlopen` of NSS modules during DNS
/// resolves normally. `assert_static` proves an empty STARTUP closure (no
/// PT_INTERP/DT_NEEDED/run-path — the flake fix), NOT that DNS needs zero runtime
/// DSOs. td-subst's code compiles into td-net, but the td-subst APPLET is deliberately
/// NOT resolved here (host_net_applet is called only for the fetch/feed applets): it is
/// sourced ambiently (`TD_SUBST_BIN`/PATH) and runs inside
/// the NEWNET-isolated loop sandbox, so its name-resolution/NSS posture is a separate
/// question (PR #534 discussion) — statically linking that path is out of scope.
///
/// Unlike the pure-std tools, the network crates (ureq/rustls/ring) pull in
/// PROC-MACROS that must compile for the host compiler, so `+crt-static` cannot
/// go in a global RUSTFLAGS (it would try to statically link the proc-macro
/// dylibs — "does not support these crate types"). Instead pass `--target
/// x86_64-unknown-linux-musl` and set the static flags via CARGO_ENCODED_RUSTFLAGS
/// (`stage0::musl_static_encoded_rustflags`): with `--target` set they apply to the
/// MUSL_TARGET binary + its normal deps ONLY, leaving host-kind build scripts /
/// proc-macros dynamic. The MUSL_TARGET link uses rustc's bundled `rust-lld` and
/// musl's self-contained `libc.a` (no external glibc, no linker-glibc matching), so
/// the result is fully static with an EMPTY runtime closure. CARGO_ENCODED_RUSTFLAGS
/// is cargo's HIGHEST-precedence flag source — the one form a guix cargo wrapper
/// (which re-injects `RUSTFLAGS="… -C linker=<gcc> -rpath …"` at runtime) cannot
/// outrank; a per-target CARGO_TARGET_<musl>_RUSTFLAGS would lose to that global
/// RUSTFLAGS, dropping `rust-lld` and baking a mutable guix-home DT_RUNPATH that
/// fails assert_static. The compiler is pinned too: RUSTC to the provisioned rustc and
/// RUSTC_WRAPPER/RUSTC_WORKSPACE_WRAPPER removed, so no ambient rustc or wrapper
/// interposes on the control-plane build. ring's `cc-rs` build script compiles
/// ring's C/asm FOR the musl target, so its CC/AR env forms (CC/HOST_CC/TARGET_CC
/// and the per-target CC_<musl> spellings → the toolchain's `gcc`, AR alongside —
/// every form cc-rs consults is pinned so an ambient one cannot outrank them). The
/// HOST build scripts / proc-macros still link with the provisioned cc via
/// CARGO_TARGET_<host-triple>_LINKER; `cc` may be absent by that name (a guix
/// profile exposes only `gcc`), so it is pinned explicitly.
pub(crate) fn host_cargo_bin(root: &Path, dir: &str, bin: &str, deadline: Option<Instant>) -> Option<PathBuf> {
    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let rustpath = match crate::stage0::provision_rust(&penv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: no rust toolchain ({e}) — skipping host build");
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
    let musl = crate::stage0::MUSL_TARGET;
    // The MUSL_TARGET binary gets the static flags (via CARGO_ENCODED_RUSTFLAGS —
    // see the doc comment); the host build-script link gets the provisioned cc as
    // its per-target linker.
    let host_linker_var = crate::stage0::target_linker_var(&host_triple);
    let encoded_rustflags = crate::stage0::musl_static_encoded_rustflags();
    // cc-rs (ring's C build) resolves the compiler/archiver from the FIRST of
    // several env forms, and the per-target-suffixed forms outrank the plain
    // CC/HOST_CC/AR we set. ring's C compiles FOR the musl target, so pin every
    // form cc-rs consults for that triple (both dash and underscore spellings) to
    // the matched toolchain so an ambient CC_<triple>/AR_<triple>/HOST_AR cannot
    // slip a different compiler into a control-plane binary (review PR #534).
    let musl_us = musl.replace('-', "_");
    let cc_target = format!("CC_{musl}");
    let cc_target_us = format!("CC_{musl_us}");
    let ar_target = format!("AR_{musl}");
    let ar_target_us = format!("AR_{musl_us}");
    let ambient_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{rustpath}:{ccpath}:{ambient_path}");

    let mut command = Command::new(&cargo);
    command
        .args(["build", "--release", "--quiet", "--target", musl])
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
    arm_check_child(&mut command);
    let child = command.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("td-builder check: static {bin}: cannot spawn cargo ({e}) — skipping host build");
            return None;
        }
    };
    if !wait_with_deadline(&mut child, deadline) {
        return None;
    }
    let p = root
        .join(dir)
        .join("target")
        .join(musl)
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
pub(crate) fn wait_with_deadline(child: &mut std::process::Child, deadline: Option<Instant>) -> bool {
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
