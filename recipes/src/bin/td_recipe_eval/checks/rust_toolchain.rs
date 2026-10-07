use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use td_recipe::catalog;
use td_recipe::types::CheckRunner;

use crate::check_runner::{is_executable, RecipeCheckRunner, TD_STORE_DIR};
use crate::sha256::Sha256;

// The nested `stage/td/store/<pkg>` prefix each `-self`/glibc recipe output carries.
// `td-builder`'s NATIVE_GCC_STAGE/toolchain_x86_64::GLIBC_X86_64_STAGE embed the
// same pinned versions for the build-plan `--auto` link env; the two must move
// together on a compiler/libc bump (re #547).
const GCC_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64-self";
pub(super) const GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

/// The userland the `td shell` product proof builds and exercises, in the
/// order `CheckRunner::RustToolchain` declares them.
const PROVED_USERLAND: &[&str] = &["ripgrep", "fd", "uutils"];

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("rust-toolchain")?;
    let build_out = runner.build_plan("rust-toolchain")?;
    let rust_tree = runner.ladder_out_from(&build_out, "rust-toolchain")?;
    let stage0_tree = runner.ladder_out_from(&build_out, "rust-stage0")?;
    let gcc_tree = runner.ladder_out_from(&build_out, "gcc-x86-64-self")?;
    let binutils_tree = runner.ladder_out_from(&build_out, "binutils-x86-64-self")?;
    let glibc_tree = runner.ladder_out_from(&build_out, "glibc-x86-64")?;
    // The proof's shell and text tools are the bootstrap root's userland, which
    // the toolchain's own build ran on: nothing later exists before this check.
    let sh = format!(
        "{}/sh",
        root_userland_bin(runner, &build_out, "bash-mesboot")?
    );
    let userland = ROOT_USERLAND
        .iter()
        .map(|stem| root_userland_bin(runner, &build_out, stem))
        .collect::<Result<Vec<_>, _>>()?
        .join(":");
    println!(
        "   [ladder] x86_64 Rust bridge via build-plan --auto: exact stage0 snapshot -> source-built rustc/std/Cargo/Clippy ({})",
        rust_tree.display()
    );
    for binary in ["rustc", "rustdoc", "cargo", "cargo-clippy", "clippy-driver"] {
        let path = rust_tree.join("bin").join(binary);
        if !is_executable(&path) {
            return Err(format!(
                "{binary} missing from rust-toolchain output ({})",
                path.display()
            ));
        }
    }

    reject_stage0_artifacts(&stage0_tree, &rust_tree)?;
    reject_shared_llvm(&rust_tree)?;

    let rust_base = path_basename(&rust_tree)?;
    let gcc_base = path_basename(&gcc_tree)?;
    let binutils_base = path_basename(&binutils_tree)?;
    let glibc_base = path_basename(&glibc_tree)?;
    let rust_path = format!("{TD_STORE_DIR}/{rust_base}");
    let gcc_path = format!("{TD_STORE_DIR}/{gcc_base}/{GCC_STAGE}");
    let binutils_path = format!("{TD_STORE_DIR}/{binutils_base}/bin");
    let glibc_path = format!("{TD_STORE_DIR}/{glibc_base}/{GLIBC_STAGE}");

    let rustc_version =
        runner.store_ns_output(&[&format!("{rust_path}/bin/rustc"), "--version"], None)?;
    if !rustc_version.starts_with("rustc 1.96.0") {
        return Err(format!(
            "rustc version did not match the pinned 1.96.0 release: {}",
            rustc_version.trim()
        ));
    }
    let cargo_version =
        runner.store_ns_output(&[&format!("{rust_path}/bin/cargo"), "--version"], None)?;
    if !cargo_version.starts_with("cargo 1.96.0") {
        return Err(format!(
            "Cargo version did not match the source-built 1.96.0 release: {}",
            cargo_version.trim()
        ));
    }

    let smoke = format!(
        "set -eu\n\
         test ! -e /gnu/store\n\
         export PATH='{userland}'\n\
         readelf='{binutils_path}/readelf'\n\
         test -f '{rust_path}/share/td/debug-size'\n\
         grep -F -x 'scope=rust-toolchain' '{rust_path}/share/td/debug-size' >/dev/null\n\
         marker='{rust_path}/lib/debug/.td-assembly-exception'\n\
         test -f \"$marker\"\n\
         grep -F -x 'exception.2.source=rust-toolchain' \"$marker\" >/dev/null\n\
         line_marker='{rust_path}/lib/debug/.td-line-attribution-exception'\n\
         driver_runtime='{rust_path}/lib/librustc_driver-277b25caa34f5853.so'\n\
         driver_debug='{rust_path}/lib/debug/lib/librustc_driver-277b25caa34f5853.so.debug'\n\
         test -f \"$line_marker\"\n\
         test -f \"$driver_runtime\"\n\
         test -f \"$driver_debug\"\n\
         driver_runtime_id=\"$(\"$readelf\" -n \"$driver_runtime\" | grep 'Build ID:')\"\n\
         driver_debug_id=\"$(\"$readelf\" -n \"$driver_debug\" | grep 'Build ID:')\"\n\
         test -n \"$driver_runtime_id\" || exit 83\n\
         test \"$driver_runtime_id\" = \"$driver_debug_id\" || exit 84\n\
         rc=0; \"$readelf\" -S \"$driver_runtime\" | grep -F '.symtab' >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 87\n\
         \"$readelf\" -S \"$driver_debug\" | grep -F '.symtab' >/dev/null\n\
         line_size=\"$(\"$readelf\" -SW \"$driver_debug\" | awk '{{ for (i = 1; i <= NF; i++) if ($i == \".debug_line\") {{ print $(i + 4); exit }} }}')\"\n\
         printf '%s\\n' \"$line_size\" | grep -E '^0*6179aa9$' >/dev/null || exit 85\n\
         rc=0; \"$readelf\" -SW \"$driver_debug\" | grep -E '[[:space:]](\\.debug_(info|abbrev|aranges|types|ranges|rnglists|frame|loc|loclists|str|str_offsets|addr|macro|macinfo|pubnames|pubtypes|gnu_pubnames|gnu_pubtypes|names|sup|cu_index|tu_index)|\\.gdb_index)([[:space:]]|$)' >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 88\n\
         test \"$(wc -c < \"$driver_debug\")\" = 165003688 || exit 86\n\
         grep -F -x 'output=rust-toolchain' \"$line_marker\" >/dev/null\n\
         grep -F -x 'runtime=lib/librustc_driver-277b25caa34f5853.so' \"$line_marker\" >/dev/null\n\
         grep -F -x 'reader_ceiling_bytes=33554432' \"$line_marker\" >/dev/null\n\
         grep -F -x 'admitted_ceiling_bytes=134217728' \"$line_marker\" >/dev/null\n\
         grep -F -x 'companion_ceiling_bytes=201326592' \"$line_marker\" >/dev/null\n\
         grep -F \"Rust 1.96.0 librustc_driver's line program\" \"$line_marker\" >/dev/null\n\
         for name in rustc rustdoc cargo cargo-clippy clippy-driver; do\n\
           runtime='{rust_path}/bin/'\"$name\"\n\
           debug='{rust_path}/lib/debug/bin/'\"$name\"'.debug'\n\
           test -f \"$debug\"\n\
           test \"$(\"$readelf\" -n \"$runtime\" | grep 'Build ID:')\" = \"$(\"$readelf\" -n \"$debug\" | grep 'Build ID:')\"\n\
           rc=0; \"$readelf\" -S \"$runtime\" | grep -F '.symtab' >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 89\n\
           \"$readelf\" -S \"$debug\" | grep -F '.symtab' >/dev/null\n\
           \"$readelf\" -S \"$debug\" | grep -F '.debug_line' >/dev/null\n\
         done\n\
         printf '%s\\n' 'fn main() {{ println!(\"42\"); }}' >/tmp/td-rust-smoke.rs\n\
         '{rust_path}/bin/rustc' --edition=2021 /tmp/td-rust-smoke.rs \
           -C linker={gcc_path}/bin/gcc \
           -C link-arg=-B{binutils_path}/ \
           -C link-arg=-B{glibc_path}/lib \
           -C link-arg=-L{glibc_path}/lib \
           -C link-arg=-static-libgcc \
           -C link-arg=-Wl,--dynamic-linker,{glibc_path}/lib/ld-linux-x86-64.so.2 \
           -C link-arg=-Wl,--enable-new-dtags \
           -C link-arg=-Wl,-rpath,{glibc_path}/lib \
           -o /tmp/td-rust-smoke\n\
         test \"$(/tmp/td-rust-smoke)\" = 42\n\
         mkdir -p /tmp/td-cargo-smoke/src /tmp/td-cargo-home /tmp/td-cargo-target\n\
         printf '%s\\n' '[package]' 'name = \"td-cargo-smoke\"' 'version = \"0.0.0\"' 'edition = \"2021\"' >/tmp/td-cargo-smoke/Cargo.toml\n\
         printf '%s\\n' 'fn main() {{ println!(\"43\"); }}' >/tmp/td-cargo-smoke/src/main.rs\n\
         export PATH='{rust_path}/bin'\n\
         RUSTC='{rust_path}/bin/rustc' \
         CARGO_HOME=/tmp/td-cargo-home \
         HOME=/tmp/td-cargo-home \
         CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER='{gcc_path}/bin/gcc' \
         RUSTFLAGS='-C link-arg=-B{binutils_path}/ -C link-arg=-B{glibc_path}/lib -C link-arg=-L{glibc_path}/lib -C link-arg=-static-libgcc -C link-arg=-Wl,--dynamic-linker,{glibc_path}/lib/ld-linux-x86-64.so.2 -C link-arg=-Wl,--enable-new-dtags -C link-arg=-Wl,-rpath,{glibc_path}/lib' \
         '{rust_path}/bin/cargo' build --offline --manifest-path /tmp/td-cargo-smoke/Cargo.toml --target-dir /tmp/td-cargo-target\n\
         test \"$(/tmp/td-cargo-target/debug/td-cargo-smoke)\" = 43\n\
         printf '%s\\n' CARGO-BRIDGE-OK\n\
         printf '%s\\n' RUST-BRIDGE-OK\n"
    );
    let smoke_out = runner.store_ns_output(&[&sh, "-c", &smoke], None)?;
    if !smoke_out.lines().any(|line| line == "RUST-BRIDGE-OK")
        || !smoke_out.lines().any(|line| line == "CARGO-BRIDGE-OK")
    {
        return Err(format!(
            "source-built stage2 rustc/Cargo did not complete their td-native smoke tests: {}",
            smoke_out.trim()
        ));
    }
    prove_td_shell_userland(
        runner,
        &build_out,
        &stage0_tree,
        &gcc_path,
        &binutils_path,
        &glibc_path,
        &sh,
        &userland,
        rust_base,
        gcc_base,
        binutils_base,
        glibc_base,
    )?;
    println!(
        "PASS: rust-toolchain: source-built Rust 1.96.0 rustc/std/Cargo/Clippy contain no stage0 artifacts; td shell builds (or reuses from its cache) and runs ripgrep/fd/uutils against td GCC/glibc with /gnu/store absent"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prove_td_shell_userland(
    runner: &RecipeCheckRunner,
    build_out: &Path,
    stage0_tree: &Path,
    gcc_path: &str,
    binutils_path: &str,
    glibc_path: &str,
    sh: &str,
    userland: &str,
    rust_base: &str,
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

    let dbs = runner.recipe_output_dbs(build_out)?;
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
    let stage0_base = path_basename(stage0_tree)?;
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
           grep -F -x 'exception.1.source=gcc-x86-64-self' \"$marker\" >/dev/null\n\
           grep -F -x 'exception.2.source=rust-toolchain' \"$marker\" >/dev/null\n\
           rc=0; grep -a -F /gnu/store \"$binary\" >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 93\n\
           rc=0; grep -a -F '{stage0_base}' \"$binary\" >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 94\n\
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
    // A toolchain built above the pinned root references root items, which
    // only the root db vouches; td-builder authenticates it at intake.
    if let Some(db) = runner.bootstrap_root_db_for("rust-toolchain")? {
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
        "   [product] td shell built (or reused from its cache) ripgrep {rg_version}, fd {fd_version}, and uutils {uutils_version} with source-built stage2 and ran all three under own-root /td/store"
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
/// root's item when the plan started at the cut, which stages it into the
/// check's store, else the plan's own build of the rung from stage0.
fn root_userland_bin(
    runner: &RecipeCheckRunner,
    build_out: &Path,
    stem: &str,
) -> Result<String, String> {
    let base = if runner.bootstrap_root_db_for("rust-toolchain")?.is_some() {
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

/// Reject both direct references to the stage0 store path and byte-for-byte copies
/// of compiled stage0 artifacts. Text/debugger helpers may legitimately be
/// reproduced from the same Rust source, so the equality oracle is deliberately
/// scoped to executables and compiler/linker artifacts.
fn reject_stage0_artifacts(stage0: &Path, final_tree: &Path) -> Result<(), String> {
    let stage0_base = path_basename(stage0)?.as_bytes();
    let mut stage0_hashes = HashMap::new();
    for path in regular_files(stage0)? {
        if is_compiled_artifact(&path)? {
            stage0_hashes.insert(hash_file(&path)?, path);
        }
    }
    for path in tree_entries(final_tree)? {
        let meta =
            fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            let target =
                fs::read_link(&path).map_err(|e| format!("readlink {}: {e}", path.display()))?;
            if target
                .as_os_str()
                .as_encoded_bytes()
                .windows(stage0_base.len())
                .any(|part| part == stage0_base)
            {
                return Err(format!(
                    "final Rust toolchain symlink {} references rust-stage0 ({})",
                    path.display(),
                    target.display()
                ));
            }
        } else if meta.is_file() {
            let compiled = is_compiled_artifact(&path)?;
            let (digest, contains_stage0) = hash_file_and_contains(&path, stage0_base)?;
            if contains_stage0 {
                return Err(format!(
                    "final Rust toolchain file {} contains the rust-stage0 store basename",
                    path.display()
                ));
            }
            if compiled {
                if let Some(source) = stage0_hashes.get(&digest) {
                    return Err(format!(
                        "final Rust toolchain copied compiled stage0 artifact {} byte-for-byte as {}",
                        source.display(),
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn reject_shared_llvm(root: &Path) -> Result<(), String> {
    for path in tree_entries(root)? {
        let name = path
            .file_name()
            .and_then(|part| part.to_str())
            .unwrap_or("");
        if name.starts_with("libLLVM") && name.contains(".so") {
            return Err(format!(
                "final Rust toolchain contains shared/prebuilt LLVM artifact {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn is_compiled_artifact(path: &Path) -> Result<bool, String> {
    let name = path
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or("");
    if name.ends_with(".rlib")
        || name.ends_with(".a")
        || name.ends_with(".o")
        || name.ends_with(".so")
        || name.contains(".so.")
    {
        return Ok(true);
    }
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    let len = file
        .read(magic.as_mut_slice())
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(len == magic.len() && magic == *b"\x7fELF")
}

fn regular_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for path in tree_entries(root)? {
        let meta =
            fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

fn tree_entries(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = Vec::new();
    while let Some(dir) = pending.pop() {
        let mut children = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| format!("read dir {}: {e}", dir.display()))? {
            children.push(
                entry
                    .map_err(|e| format!("read dir {} entry: {e}", dir.display()))?
                    .path(),
            );
        }
        children.sort();
        for path in children {
            let meta =
                fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
            if meta.is_dir() {
                pending.push(path.clone());
            }
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}

fn hash_file(path: &Path) -> Result<[u8; 32], String> {
    Ok(hash_file_and_contains(path, &[])?.0)
}

/// Hash a file and scan for a byte sequence in one pass. Final Rust/LLVM
/// artifacts are large enough that reading each once keeps the trust-root
/// check bounded without weakening either oracle.
fn hash_file_and_contains(path: &Path, needle: &[u8]) -> Result<([u8; 32], bool), String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut carry = Vec::new();
    let mut contains = needle.is_empty();
    loop {
        let len = file
            .read(buffer.as_mut_slice())
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if len == 0 {
            break;
        }
        let bytes = buffer
            .get(..len)
            .ok_or_else(|| format!("invalid read length {len} for {}", path.display()))?;
        hasher.update(bytes);
        if !contains {
            let mut scan = Vec::with_capacity(carry.len() + bytes.len());
            scan.extend_from_slice(&carry);
            scan.extend_from_slice(bytes);
            contains = scan.windows(needle.len()).any(|part| part == needle);
            let keep = needle.len().saturating_sub(1).min(scan.len());
            carry.clear();
            if let Some(tail) = scan.get(scan.len().saturating_sub(keep)..) {
                carry.extend_from_slice(tail);
            }
        }
    }
    Ok((hasher.finalize(), contains))
}

#[cfg(test)]
mod tests {
    use super::{hash_file_and_contains, is_compiled_artifact, Sha256};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn combined_hash_scan_finds_a_needle_across_buffer_chunks() {
        let path = std::env::temp_dir().join(format!("td-rust-hash-scan-{}", std::process::id()));
        let mut bytes = vec![b'x'; 64 * 1024 - 2];
        bytes.extend_from_slice(b"stage0-store-name");
        fs::write(&path, &bytes).unwrap();

        let mut expected = Sha256::new();
        expected.update(&bytes);
        let (digest, found) = hash_file_and_contains(&path, b"stage0-store-name").unwrap();
        let (_, absent) = hash_file_and_contains(&path, b"not-present").unwrap();
        assert_eq!(digest, expected.finalize());
        assert!(found);
        assert!(!absent);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn compiled_artifact_classifier_uses_format_not_exec_bit() {
        let base = std::env::temp_dir().join(format!("td-rust-artifact-{}", std::process::id()));
        let script = base.with_extension("sh");
        let elf = base.with_extension("bin");
        fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&elf, b"\x7fELFtest").unwrap();

        assert!(!is_compiled_artifact(&script).unwrap());
        assert!(is_compiled_artifact(&elf).unwrap());

        fs::remove_file(script).unwrap();
        fs::remove_file(elf).unwrap();
    }

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
