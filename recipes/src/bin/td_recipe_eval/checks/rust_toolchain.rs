use std::fs;
use std::path::Path;
use std::process::Command;

use td_recipe::catalog;
use td_recipe::types::CheckRunner;

use crate::check_runner::{is_executable, RecipeCheckRunner, TD_STORE_DIR};

// The nested `stage/td/store/<pkg>` prefix each `-self`/glibc recipe output carries.
// `td-builder`'s NATIVE_GCC_STAGE/toolchain_x86_64::GLIBC_X86_64_STAGE embed the
// same pinned versions for the build-plan `--auto` link env; the two must move
// together on a compiler/libc bump (re #547).
const GCC_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64-self";
pub(super) const GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

/// The userland the `td shell` product proof builds and exercises, in the
/// order `CheckRunner::RustToolchain` declares them.
const PROVED_USERLAND: &[&str] = &["ripgrep", "fd", "uutils"];

/// The shipped executables the transform relinks onto td glibc's loader.
const RELINKED: &[&str] = &[
    "bin/rustc",
    "bin/rustdoc",
    "bin/cargo",
    "bin/cargo-clippy",
    "bin/clippy-driver",
    "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
];

/// Compiles a lint-free crate and one using `unwrap()` under
/// `--deny clippy::unwrap_used`, read on stdin by the smoke below.
const CLIPPY_PROBE: &str = include_str!("../../../probes/rust_clippy.rs");

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("rust-toolchain")?;
    let build_out = runner.build_plan("rust-toolchain")?;
    let rust_tree = runner.ladder_out_from(&build_out, "rust-toolchain")?;
    let glibc_tree = runner.ladder_out_from(&build_out, "glibc-x86-64")?;
    let runtime_tree = runner.ladder_out_from(&build_out, "gcc-runtime")?;
    let zlib_tree = runner.ladder_out_from(&build_out, "zlib-x86-64")?;
    // The proofs link through td-link, as every Rust recipe does. GCC is the
    // C compiler `td shell` hands C build scripts, and binutils the readelf
    // the proofs inspect with.
    runner.prepare_recipe_target("td-link")?;
    let link_out = runner.build_plan("td-link")?;
    let link_tree = runner.ladder_out_from(&link_out, "td-link")?;
    runner.prepare_recipe_target("gcc-x86-64-self")?;
    let gcc_out = runner.build_plan("gcc-x86-64-self")?;
    let gcc_tree = runner.ladder_out_from(&gcc_out, "gcc-x86-64-self")?;
    let binutils_tree = runner.ladder_out_from(&gcc_out, "binutils-x86-64-self")?;
    // The proof's shell and text tools are the bootstrap root's userland:
    // nothing later exists before this check. The toolchain's closure is
    // built in the Debian root, so they come from GCC's plan.
    let gcc = "gcc-x86-64-self";
    let sh = format!(
        "{}/sh",
        root_userland_bin(runner, gcc, &gcc_out, "bash-mesboot")?
    );
    let userland = ROOT_USERLAND
        .iter()
        .map(|stem| root_userland_bin(runner, gcc, &gcc_out, stem))
        .collect::<Result<Vec<_>, _>>()?
        .join(":");
    println!(
        "   [ladder] x86_64 upstream Rust 1.96.0 via build-plan --auto, retargeted onto td glibc/libgcc/zlib ({})",
        rust_tree.display()
    );
    for binary in RELINKED {
        let path = rust_tree.join(binary);
        if !is_executable(&path) {
            return Err(format!(
                "{binary} missing from rust-toolchain output ({})",
                path.display()
            ));
        }
    }

    let rust_base = path_basename(&rust_tree)?;
    let gcc_base = path_basename(&gcc_tree)?;
    let binutils_base = path_basename(&binutils_tree)?;
    let glibc_base = path_basename(&glibc_tree)?;
    let link_base = path_basename(&link_tree)?;
    let td_link = format!("{TD_STORE_DIR}/{link_base}/bin/td-link");
    let rust_path = format!("{TD_STORE_DIR}/{rust_base}");
    let gcc_path = format!("{TD_STORE_DIR}/{gcc_base}/{GCC_STAGE}");
    let binutils_path = format!("{TD_STORE_DIR}/{binutils_base}/bin");
    let glibc_path = format!("{TD_STORE_DIR}/{glibc_base}/{GLIBC_STAGE}");
    let runtime_path = format!("{TD_STORE_DIR}/{}/lib", path_basename(&runtime_tree)?);
    let zlib_path = format!("{TD_STORE_DIR}/{}/lib", path_basename(&zlib_tree)?);

    for (binary, version) in [("rustc", "rustc 1.96.0"), ("cargo", "cargo 1.96.0")] {
        let out =
            runner.store_ns_output(&[&format!("{rust_path}/bin/{binary}"), "--version"], None)?;
        if !out.starts_with(version) {
            return Err(format!(
                "{binary} version did not match the pinned 1.96.0 release: {}",
                out.trim()
            ));
        }
    }

    let relinked = RELINKED.join(" ");
    // td's recipes link through td-link, which runs rust-lld: every link
    // below must carry LLD's comment.
    let link = format!("-C linker={td_link}");
    let smoke = format!(
        "set -eu\n\
         test ! -e /gnu/store\n\
         export PATH='{userland}'\n\
         readelf='{binutils_path}/readelf'\n\
         interp='{glibc_path}/lib/ld-linux-x86-64.so.2'\n\
         cd '{rust_path}'\n\
         for b in {relinked}; do\n\
           \"$readelf\" -l \"$b\" | grep -F \"[Requesting program interpreter: $interp]\" >/dev/null || {{ echo \"$b is not on td glibc's loader\" >&2; exit 81; }}\n\
         done\n\
         for pair in libc.so.6:'{glibc_path}/lib/libc.so.6' libgcc_s.so.1:'{runtime_path}/libgcc_s.so.1' libz.so.1:'{zlib_path}/libz.so.1'; do\n\
           test \"$(readlink \"lib/${{pair%%:*}}\")\" = \"${{pair#*:}}\" || {{ echo \"lib/${{pair%%:*}} is not td's\" >&2; exit 82; }}\n\
         done\n\
         gccld=lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld\n\
         test \"$(readlink \"$gccld/ld.lld\")\" = ../rust-lld || exit 83\n\
         test \"$(ls \"$gccld\")\" = ld.lld || {{ echo 'gcc-ld holds more than ld.lld' >&2; exit 83; }}\n\
         \"$gccld/ld.lld\" --version | grep -F LLD >/dev/null || exit 84\n\
         test ! -e lib/debug || {{ echo 'the upstream toolchain carries debug companions' >&2; exit 85; }}\n\
         test -f share/doc/rust/COPYRIGHT.html || {{ echo 'the upstream licence notices are missing' >&2; exit 85; }}\n\
         rc=0; grep -r -a -F -l /gnu/store bin lib >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 86\n\
         cd /tmp\n\
         printf '%s\\n' 'fn main() {{ println!(\"42\"); }}' >/tmp/td-rust-smoke.rs\n\
         '{rust_path}/bin/rustc' --edition=2021 /tmp/td-rust-smoke.rs {link} -o /tmp/td-rust-ld\n\
         test \"$(/tmp/td-rust-ld)\" = 42\n\
         c=$(\"$readelf\" -p .comment /tmp/td-rust-ld) || exit 89\n\
         case $c in *'rustc version'*) ;; *) echo 'the link has no rustc comment' >&2; exit 89;; esac\n\
         case $c in *'Linker: LLD'*) ;; *) echo 'the td-link link did not use rust-lld' >&2; exit 87;; esac\n\
         mkdir -p /tmp/td-cargo-smoke/src /tmp/td-cargo-home /tmp/td-cargo-target\n\
         printf '%s\\n' '[package]' 'name = \"td-cargo-smoke\"' 'version = \"0.0.0\"' 'edition = \"2021\"' >/tmp/td-cargo-smoke/Cargo.toml\n\
         printf '%s\\n' 'fn main() {{ println!(\"43\"); }}' >/tmp/td-cargo-smoke/src/main.rs\n\
         PATH='{rust_path}/bin' RUSTC='{rust_path}/bin/rustc' CARGO_HOME=/tmp/td-cargo-home HOME=/tmp/td-cargo-home \
         CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER='{td_link}' \
         '{rust_path}/bin/cargo' build --offline --manifest-path /tmp/td-cargo-smoke/Cargo.toml --target-dir /tmp/td-cargo-target\n\
         test \"$(/tmp/td-cargo-target/debug/td-cargo-smoke)\" = 43\n\
         mkdir -p /tmp/td-clippy-proof\n\
         cat >/tmp/td-clippy-proof/probe.rs\n\
         '{rust_path}/bin/rustc' --edition=2021 /tmp/td-clippy-proof/probe.rs {link} -o /tmp/td-clippy-proof/probe\n\
         (cd /tmp/td-clippy-proof && PATH='{rust_path}/bin' ./probe '{rust_path}/bin/cargo' '{rust_path}/bin/rustc' '{td_link}')\n\
         printf '%s\\n' RUST-TOOLCHAIN-OK\n"
    );
    let smoke_out = runner.store_ns_output(&[&sh, "-c", &smoke], Some(CLIPPY_PROBE))?;
    if !smoke_out.lines().any(|line| line == "RUST-TOOLCHAIN-OK") {
        return Err(format!(
            "upstream rustc/Cargo/Clippy did not complete their td-native smoke tests: {}",
            smoke_out.trim()
        ));
    }
    prove_td_shell_userland(
        runner,
        &[&build_out, &link_out, &gcc_out],
        &gcc_path,
        &binutils_path,
        &glibc_path,
        &sh,
        &userland,
        rust_base,
        link_base,
        gcc_base,
        binutils_base,
        glibc_base,
    )?;
    println!(
        "PASS: rust-toolchain: upstream Rust 1.96.0 rustc/Cargo/Clippy/rust-lld run on td glibc, gcc-runtime and zlib, link through td-link and rust-lld, and td shell builds (or reuses from its cache) and runs ripgrep/fd/uutils with /gnu/store absent"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prove_td_shell_userland(
    runner: &RecipeCheckRunner,
    build_outs: &[&Path],
    gcc_path: &str,
    binutils_path: &str,
    glibc_path: &str,
    sh: &str,
    userland: &str,
    rust_base: &str,
    link_base: &str,
    gcc_base: &str,
    binutils_base: &str,
    glibc_base: &str,
) -> Result<(), String> {
    let root = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let vendor_root = root.join(".td-build-cache/crate-vendor");
    // The packages are the runner's declared builds, so the key and reach
    // that read them name exactly what is built here; their versions come
    // from the recipes, so a bump needs no edit to this proof.
    let packages = CheckRunner::RustToolchain.extra_builds();
    // The script below exercises each package in its own role; a declared
    // build it does not exercise, or a role it lost, is a drift to fix here.
    if packages != PROVED_USERLAND {
        return Err(format!(
            "rust-toolchain declares the builds [{}] but its product proof exercises \
             [{}]; change both together",
            packages.join(" "),
            PROVED_USERLAND.join(" ")
        ));
    }
    let version = |stem: &str| {
        catalog::lookup(stem)
            .map(|r| r.version)
            .ok_or_else(|| format!("no recipe `{stem}' for the td shell product proof"))
    };
    let (rg_version, fd_version, uutils_version) =
        (version("ripgrep")?, version("fd")?, version("uutils")?);
    for package in packages {
        let package_root = vendor_root.join(package);
        if !package_root.join("work").is_dir() || !package_root.join("vendor").is_dir() {
            return Err(format!(
                "td shell Rust closure for `{package}' is not warm under {}",
                package_root.display()
            ));
        }
    }

    let product = runner.product_scratch("td-shell-userland");
    fs::create_dir_all(product.join("tmp"))
        .map_err(|e| format!("create {}: {e}", product.display()))?;
    let native_lock = product.join("native.lock");
    let lock = format!(
        "rust-toolchain {TD_STORE_DIR}/{rust_base} td-recipe-output\n\
         td-link {TD_STORE_DIR}/{link_base} td-recipe-output\n\
         gcc-x86-64-self {TD_STORE_DIR}/{gcc_base} td-recipe-output\n\
         binutils-x86-64-self {TD_STORE_DIR}/{binutils_base} td-recipe-output\n\
         glibc-x86-64 {TD_STORE_DIR}/{glibc_base} td-recipe-output\n"
    );
    fs::write(&native_lock, &lock).map_err(|e| format!("write {}: {e}", native_lock.display()))?;

    // `td shell`'s build cache outlives the run, beside the ladder, so an
    // unchanged package is a content-addressed hit rather than a rebuild;
    // the hit is committed into this run's own store as a build would be.
    // Held exclusively: checks share the ladder, and two `td shell`s must
    // not build into one cache. Emptied when the toolchain moves, since
    // every package's derivation moves with it.
    let lw = runner.ladder_work_dir();
    let shell_cache = lw.join(SHELL_CACHE_DIR);
    let shell_cache_lock = lw.join(format!("{SHELL_CACHE_DIR}.lock"));
    // A peer's proof can hold it for a cold build; say so rather than sit
    // silent. The probe is only the message: `lock_file` takes the lock.
    let busy = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&shell_cache_lock)
        .is_ok_and(|probe| matches!(probe.try_lock(), Err(fs::TryLockError::WouldBlock)));
    if busy {
        eprintln!(
            "   [product] waiting for another check's td shell proof to release {}",
            shell_cache_lock.display()
        );
    }
    let _shell_cache_lock = crate::check_runner::lock_file(&shell_cache_lock)?;
    reset_shell_cache_on_toolchain_change(&shell_cache, &lock)?;

    // The toolchain's, td-link's and GCC's plans each vouch their own outputs.
    let mut dbs = Vec::new();
    for build_out in build_outs {
        for db in runner.recipe_output_dbs(build_out)? {
            if !dbs.contains(&db) {
                dbs.push(db);
            }
        }
    }
    let dbs = dbs
        .iter()
        .map(|path| {
            path.to_str()
                .ok_or_else(|| format!("non-UTF-8 recipe-output database: {}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?
        .join(":");
    let tdstore = runner.tdstore_path();
    let store_ns_builder = runner.control_builder_path();
    let evaluator = std::env::current_exe().map_err(|e| format!("locate td-recipe-eval: {e}"))?;
    let interp = format!("{glibc_path}/lib/ld-linux-x86-64.so.2");
    let script = format!(
        "set -eu\n\
         test ! -e /gnu/store\n\
         export PATH='{userland}'\n\
         rg=''\n\
         for p in {TD_STORE_DIR}/*-ripgrep-{rg_version}/bin/rg; do\n\
           if test -x \"$p\"; then rg=$p; fi\n\
         done\n\
         fd=''\n\
         for p in {TD_STORE_DIR}/*-fd-{fd_version}/bin/fd; do\n\
           if test -x \"$p\"; then fd=$p; fi\n\
         done\n\
         uutils=''\n\
         for p in {TD_STORE_DIR}/*-uutils-{uutils_version}/bin/coreutils; do\n\
           if test -x \"$p\"; then uutils=$p; fi\n\
         done\n\
         test -n \"$rg\"\n\
         test -n \"$fd\"\n\
         test -n \"$uutils\"\n\
         mkdir -p /tmp/td-shell-userland/sub\n\
         printf '%s\\n' TD-414-NEEDLE >/tmp/td-shell-userland/sub/known-needle.txt\n\
         rg_out=$(\"$rg\" --fixed-strings TD-414-NEEDLE /tmp/td-shell-userland)\n\
         case \"$rg_out\" in *TD-414-NEEDLE*) ;; *) exit 91 ;; esac\n\
         fd_out=$(\"$fd\" '^known-needle[.]txt$' /tmp/td-shell-userland)\n\
         case \"$fd_out\" in */known-needle.txt) ;; *) exit 92 ;; esac\n\
         uu_out=$(\"$uutils\" printf '%s:%s\\n' TD UUTILS)\n\
         test \"$uu_out\" = TD:UUTILS || exit 95\n\
         test \"$(\"$uutils\" cat /tmp/td-shell-userland/sub/known-needle.txt)\" = TD-414-NEEDLE || exit 96\n\
         test \"$(\"$uutils\" uname -s)\" = Linux || exit 97\n\
         uu_id=$(\"$uutils\" id -u)\n\
         case \"$uu_id\" in ''|*[!0-9]*) exit 98 ;; esac\n\
         \"$uutils\" --list >/tmp/td-shell-userland/uutils-list || exit 99\n\
         rc=0; grep -F -x stdbuf /tmp/td-shell-userland/uutils-list >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 100\n\
         readelf='{binutils_path}/readelf'\n\
         \"$readelf\" -l \"$rg\" | grep -F '{interp}' >/dev/null\n\
         \"$readelf\" -l \"$fd\" | grep -F '{interp}' >/dev/null\n\
         \"$readelf\" -l \"$uutils\" | grep -F '{interp}' >/dev/null\n\
         for binary in \"$rg\" \"$fd\" \"$uutils\"; do\n\
           package=${{binary%/bin/*}}\n\
           name=${{binary##*/}}\n\
           debug=\"$package/lib/debug/bin/$name.debug\"\n\
           test -f \"$debug\"\n\
           test \"$(\"$readelf\" -n \"$binary\" | grep 'Build ID:')\" = \"$(\"$readelf\" -n \"$debug\" | grep 'Build ID:')\"\n\
           rc=0; \"$readelf\" -S \"$binary\" | grep -F '.symtab' >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 88\n\
           \"$readelf\" -S \"$debug\" | grep -F '.symtab' >/dev/null\n\
           \"$readelf\" -S \"$debug\" | grep -F '.debug_line' >/dev/null\n\
           marker=\"$package/lib/debug/.td-assembly-exception\"\n\
           grep -F -x 'exception.0.source=glibc-x86-64' \"$marker\" >/dev/null\n\
           grep -F -x 'exception.1.source=gcc-runtime' \"$marker\" >/dev/null\n\
           grep -F -x 'exception.2.source=rust-toolchain' \"$marker\" >/dev/null\n\
           rc=0; grep -a -F /gnu/store \"$binary\" >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 93\n\
         done\n\
         printf '%s\\n' TD-SHELL-USERLAND-OK\n"
    );

    let tdstore_s = path_str(&tdstore)?;
    let product_s = path_str(&product)?;
    let tmp = product.join("tmp");
    let tmp_s = path_str(&tmp)?;
    let vendor_s = path_str(&vendor_root)?;
    let repo_s = path_str(&root)?;
    let lock_s = path_str(&native_lock)?;
    let persist_db = product.join("products.db");
    let persist_db_s = path_str(&persist_db)?;
    let store_ns_builder_s = path_str(store_ns_builder)?;
    let evaluator_s = path_str(&evaluator)?;
    let mut cmd: Command = runner.clean_builder_command();
    cmd.env("HOME", product_s)
        .env("TMPDIR", tmp_s)
        .env("PATH", "")
        .env("TD_RECIPE_EVAL", evaluator_s)
        .env("TD_SHELL_CACHE", &shell_cache)
        .env("TD_SHELL_VENDOR_ROOT", vendor_s)
        .env("TD_SHELL_REPO_ROOT", repo_s)
        .env("TD_SHELL_NATIVE_STORE", tdstore_s)
        .env("TD_SHELL_NATIVE_EXTRA_DBS", dbs)
        .env("TD_SHELL_NATIVE_INTERP", &interp)
        .env("TD_SHELL_NATIVE_RPATH", format!("{glibc_path}/lib"))
        .env(
            "TD_SHELL_NATIVE_BDIR",
            format!("{binutils_path}:{glibc_path}/lib"),
        )
        .env("TD_SHELL_NATIVE_CC", format!("{gcc_path}/bin/gcc"))
        .env("TD_SHELL_NATIVE_CXX", format!("{gcc_path}/bin/g++"))
        .env("TD_SHELL_NATIVE_INCLUDE", format!("{glibc_path}/include"))
        .env("TD_SHELL_NATIVE_LOCK", lock_s)
        .env("TD_PERSIST_STORE", tdstore_s)
        .env("TD_PERSIST_DB", persist_db_s);
    // The GCC built above the pinned root references root items, which
    // only the root db vouches; td-builder authenticates it at intake.
    if let Some(db) = runner.bootstrap_root_db_for("gcc-x86-64-self")? {
        cmd.env("TD_SHELL_NATIVE_ROOT_DB", path_str(&db)?);
    }
    // The shell asks this evaluator for each recipe it builds; the command
    // carries the check's confinement, so it refuses any not declared.
    cmd.arg("shell")
        .args(packages)
        .args(["--", store_ns_builder_s])
        .arg("store-ns")
        .arg(tdstore_s)
        .args(["--", sh, "-c", &script]);
    let output = cmd
        .output()
        .map_err(|e| format!("spawn td shell ripgrep fd uutils product proof: {e}"))?;
    fs::write(product.join("stdout"), &output.stdout)
        .map_err(|e| format!("write td shell stdout: {e}"))?;
    fs::write(product.join("stderr"), &output.stderr)
        .map_err(|e| format!("write td shell stderr: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(format!(
            "td shell ripgrep/fd/uutils product proof failed ({}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if !stdout.lines().any(|line| line == "TD-SHELL-USERLAND-OK") {
        return Err(format!(
            "td shell ripgrep/fd/uutils product proof emitted no success marker: {}",
            stdout.trim()
        ));
    }
    println!(
        "   [product] td shell built (or reused from its cache) ripgrep {rg_version}, fd {fd_version}, and uutils {uutils_version} with upstream Rust and ran all three under own-root /td/store"
    );
    Ok(())
}

/// `td shell`'s persistent build cache for the product proof, under the
/// ladder work dir: `clear-store` drops it with the rest, `gc-store` leaves it.
const SHELL_CACHE_DIR: &str = "td-shell-cache";

/// The toolchain lock the cache's builds were made against, in the cache.
const SHELL_CACHE_STAMP: &str = "toolchain.lock";

/// Empty `cache` unless it was filled against exactly `toolchain`, then
/// record `toolchain`: a build against another toolchain is another
/// derivation, which the cache would only hold beside the current one.
fn reset_shell_cache_on_toolchain_change(cache: &Path, toolchain: &str) -> Result<(), String> {
    let stamp = cache.join(SHELL_CACHE_STAMP);
    if fs::read_to_string(&stamp).is_ok_and(|held| held == toolchain) {
        return Ok(());
    }
    // Made writable first: the realized trees in it may hold directories a
    // build left without owner write.
    crate::check_runner::remove_path_if_exists(cache)?;
    fs::create_dir_all(cache).map_err(|e| format!("create {}: {e}", cache.display()))?;
    fs::write(&stamp, toolchain).map_err(|e| format!("write {}: {e}", stamp.display()))
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

/// The bootstrap root's userland on the proofs' PATH, in lookup order: `sh` is
/// bash, and coreutils 5.0, grep 2.4 and gawk serve the rest.
const ROOT_USERLAND: &[&str] = &[
    "coreutils-mesboot0",
    "grep-mesboot0",
    "gawk-mesboot0",
    "bash-mesboot",
];

/// A root userland rung's `bin` directory under `/td/store`: the pinned
/// root's item when `target`'s plan started at the cut, which stages it
/// into the check's store, else that plan's own build of the rung from
/// stage0.
fn root_userland_bin(
    runner: &RecipeCheckRunner,
    target: &str,
    build_out: &Path,
    stem: &str,
) -> Result<String, String> {
    let base = if runner.bootstrap_root_db_for(target)?.is_some() {
        crate::bootstrap_root::root()?
            .exports
            .iter()
            .find(|(export, _)| export == stem)
            .map(|(_, base)| base.clone())
            .ok_or_else(|| format!("{stem}: not exported by the bootstrap root"))?
    } else {
        path_basename(&runner.ladder_out_from(build_out, stem)?)?.to_string()
    };
    if !runner.tdstore_path().join(&base).join("bin").is_dir() {
        return Err(format!("{stem}: {base} is not staged in the check's store"));
    }
    Ok(format!("{TD_STORE_DIR}/{base}/bin"))
}

pub(super) fn path_basename(path: &Path) -> Result<&str, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("path has no UTF-8 basename: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// The shell cache survives an unchanged toolchain and is emptied, then
    /// re-stamped, when the toolchain moves or it was never stamped.
    #[test]
    fn the_shell_cache_is_kept_only_for_its_own_toolchain() {
        let cache =
            std::env::temp_dir().join(format!("td-rust-shell-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache);
        fs::create_dir_all(cache.join("ripgrep")).unwrap();
        super::reset_shell_cache_on_toolchain_change(&cache, "tc-1\n").unwrap();
        assert!(
            !cache.join("ripgrep").exists(),
            "an unstamped cache is emptied"
        );
        fs::create_dir_all(cache.join("ripgrep")).unwrap();
        super::reset_shell_cache_on_toolchain_change(&cache, "tc-1\n").unwrap();
        assert!(
            cache.join("ripgrep").exists(),
            "the same toolchain keeps it"
        );
        // A realized tree may hold a directory without owner write.
        let sealed = cache.join("ripgrep/newstore/out");
        fs::create_dir_all(&sealed).unwrap();
        fs::write(sealed.join("rg"), b"x").unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o555)).unwrap();
        super::reset_shell_cache_on_toolchain_change(&cache, "tc-2\n").unwrap();
        assert!(
            !cache.join("ripgrep").exists(),
            "another toolchain empties it"
        );
        assert_eq!(
            fs::read_to_string(cache.join(super::SHELL_CACHE_STAMP)).unwrap(),
            "tc-2\n"
        );
        fs::remove_dir_all(cache).unwrap();
    }

    /// The proof exercises exactly the builds the runner declares, so the
    /// verdict key reads what the proof builds and nothing is built unproved.
    #[test]
    fn the_proof_exercises_exactly_the_declared_builds() {
        assert_eq!(
            super::PROVED_USERLAND,
            td_recipe::types::CheckRunner::RustToolchain.extra_builds()
        );
    }
}
