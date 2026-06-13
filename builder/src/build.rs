//! autotools-build — td's own minimal build "system", in Rust (DESIGN §7.1
//! corpus-independence; plan/corpus-independence.md "own Rust builder").
//!
//! This is the REPLACEMENT for gnu-build-system's Guile phase runner. It is
//! invoked AS the derivation's `builder` by the daemon (system td-build
//! constructs that derivation with `raw-derivation`, so the .drv construction
//! stays in guix while the build LOGIC is td's, in Rust). It runs the standard
//! autotools phases directly:
//!
//!   set-paths -> unpack -> configure (--prefix=$out) -> make -> make install
//!
//! No Guile runs in the build. The environment is derived from the inputs the
//! way gnu-build-system's `set-paths` phase does, but here in Rust. The build
//! tools (tar, gcc, make, …) are the Guix toolchain — retired LAST (§5); what is
//! removed is the build-system Guile, not the toolchain.
//!
//! Inputs (env, set by system td-build):
//!   out                output prefix (the daemon sets this)
//!   TD_SRC             the source tarball (a fixed-output url-fetch)
//!   TD_INPUTS          ':'-joined store paths of the build inputs
//!   TD_CONFIGURE_FLAGS extra ./configure flags (space-separated; may be empty)

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Find an executable `name` in a ':'-joined search path; return its abs path.
fn find_in_path(path: &str, name: &str) -> Option<String> {
    for dir in path.split(':').filter(|s| !s.is_empty()) {
        let cand = format!("{dir}/{name}");
        if Path::new(&cand).is_file() {
            return Some(cand);
        }
    }
    None
}

/// The single sub-directory of `dir` (an unpacked source tree). Errors unless
/// there is exactly one — a deterministic, fail-closed "unpack" result.
fn single_subdir(dir: &str) -> Result<String, String> {
    let mut subdirs: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {dir}: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.path().is_dir() {
            subdirs.push(entry.path().to_string_lossy().into_owned());
        }
    }
    match subdirs.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(format!(
            "expected exactly one unpacked source directory, found {}",
            subdirs.len()
        )),
    }
}

/// Run a command with a CLEAN environment (`envs` only), in `cwd`, echoing it to
/// the build log. Fail-closed: a non-zero exit aborts the build.
fn run_cmd(prog: &str, args: &[&str], cwd: &str, envs: &[(String, String)]) -> Result<(), String> {
    println!(">> td-build: (cd {cwd} && {prog} {})", args.join(" "));
    let status = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(envs.iter().map(|(k, v)| (k.clone(), v.clone())))
        .status()
        .map_err(|e| format!("spawn {prog}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} {} failed: {status}", args.join(" ")))
    }
}

pub fn run() -> Result<(), String> {
    let out = env::var("out").map_err(|_| "out not set".to_string())?;
    let src = env::var("TD_SRC").map_err(|_| "TD_SRC not set".to_string())?;
    let inputs = env::var("TD_INPUTS").unwrap_or_default();
    let configure_flags = env::var("TD_CONFIGURE_FLAGS").unwrap_or_default();

    // set-paths phase (in Rust): derive PATH / C_INCLUDE_PATH /
    // CPLUS_INCLUDE_PATH / LIBRARY_PATH from the inputs' bin/include/lib dirs.
    let (mut path, mut cinc, mut cxxinc, mut lib): (
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for p in inputs.split(':').filter(|s| !s.is_empty()) {
        let push_if_dir = |sub: &str, dst: &mut Vec<String>| {
            let d = format!("{p}/{sub}");
            if Path::new(&d).is_dir() {
                dst.push(d);
            }
        };
        push_if_dir("bin", &mut path);
        push_if_dir("include", &mut cinc);
        push_if_dir("lib", &mut lib);
        push_if_dir("lib64", &mut lib);
        // C++ search path: include/c++ then include.
        push_if_dir("include/c++", &mut cxxinc);
        push_if_dir("include", &mut cxxinc);
    }
    let path = path.join(":");

    let bash = find_in_path(&path, "bash").ok_or("bash not found in TD_INPUTS")?;
    let tar = find_in_path(&path, "tar").ok_or("tar not found in TD_INPUTS")?;
    let make = find_in_path(&path, "make").ok_or("make not found in TD_INPUTS")?;

    // The build environment, the same shape gnu-build-system sets up.
    let envs: Vec<(String, String)> = vec![
        ("out".into(), out.clone()),
        ("PATH".into(), path.clone()),
        ("C_INCLUDE_PATH".into(), cinc.join(":")),
        ("CPLUS_INCLUDE_PATH".into(), cxxinc.join(":")),
        ("LIBRARY_PATH".into(), lib.join(":")),
        // configure / make sub-shells use bash (no /bin/sh in the sandbox).
        ("CONFIG_SHELL".into(), bash.clone()),
        ("SHELL".into(), bash.clone()),
        ("SOURCE_DATE_EPOCH".into(), "1".into()),
        ("HOME".into(), "/homeless-shelter".into()),
    ];

    // unpack -> the single source tree.
    run_cmd(&tar, &["xf", &src], ".", &envs)?;
    let srcdir = single_subdir(".")?;

    // configure --prefix=$out [extra flags].
    let prefix = format!("--prefix={out}");
    let mut conf: Vec<&str> = vec!["./configure", &prefix];
    conf.extend(configure_flags.split_whitespace());
    run_cmd(&bash, &conf, &srcdir, &envs)?;

    // build + install. Pass SHELL=<bash> as a make OVERRIDE (not just env): make
    // launches recipe shells via the SHELL make-variable, defaulting to /bin/sh,
    // which does not exist in the sandbox (the `po/` install rules hit this). A
    // command-line assignment overrides the Makefile AND propagates to sub-makes.
    let shell = format!("SHELL={bash}");
    run_cmd(&make, &[&shell], &srcdir, &envs)?;
    run_cmd(&make, &[&shell, "install"], &srcdir, &envs)?;
    Ok(())
}
