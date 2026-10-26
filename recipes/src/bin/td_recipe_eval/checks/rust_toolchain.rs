use std::fs;
use std::path::Path;
use std::process::Command;

use td_recipe::catalog;
use td_recipe::types::CheckRunner;

use crate::check_runner::{is_executable, RecipeCheckRunner, TD_STORE_DIR};

// The nested `stage/td/store/<pkg>` prefix the glibc recipe output carries.
// `td-builder`'s toolchain_x86_64::GLIBC_X86_64_STAGE embeds the same pinned
// version; the two must move together on a libc bump (re #547).
pub(super) const GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

/// The userland the `td shell` product proof builds and exercises, in the
/// order `CheckRunner::RustToolchain` declares them.
const PROVED_USERLAND: &[&str] = &["ripgrep", "fd", "uutils"];

/// The proof script's own tools, declared after `PROVED_USERLAND`: td-sh
/// runs it, td-util serves `mkdir` and td-txt `grep`, each a static
/// multicall at `bin/<stem>`. None of them is a program the proof grades.
const SCRIPT_USERLAND: &[&str] = &["td-sh", "td-util", "td-txt"];

// The toolchain's ELF shape, its rustc/Cargo/Clippy smokes and the shipped
// userland's ELF and debug companions are rust-toolchain-test's, graded in
// the Debian build root. This check keeps what needs the control plane:
// the toolchain runs in a host-free /td/store namespace, and `td shell`
// builds and runs the userland there.
pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("rust-toolchain")?;
    let build_out = runner.build_plan("rust-toolchain")?;
    let rust_tree = runner.ladder_out_from(&build_out, "rust-toolchain")?;
    let glibc_tree = runner.ladder_out_from(&build_out, "glibc-x86-64")?;
    // `td shell` links through td-link, as every Rust recipe does, and
    // builds in the Debian build root, whose gcc compiles C build scripts.
    runner.prepare_recipe_target("td-link")?;
    let link_out = runner.build_plan("td-link")?;
    let link_tree = runner.ladder_out_from(&link_out, "td-link")?;
    let mut tools = Vec::new();
    for stem in SCRIPT_USERLAND {
        tools.push(script_tool(runner, stem)?);
    }
    println!(
        "   [toolchain] x86_64 upstream Rust 1.99.1 via build-plan --auto, retargeted onto td glibc/libgcc/zlib ({})",
        rust_tree.display()
    );

    let rust_base = path_basename(&rust_tree)?;
    let glibc_base = path_basename(&glibc_tree)?;
    let link_base = path_basename(&link_tree)?;
    let rust_path = format!("{TD_STORE_DIR}/{rust_base}");

    for (binary, version) in [("rustc", "rustc 1.99.1"), ("cargo", "cargo 1.99.1")] {
        let out =
            runner.store_ns_output(&[&format!("{rust_path}/bin/{binary}"), "--version"], None)?;
        if !out.starts_with(version) {
            return Err(format!(
                "{binary} version did not match the pinned 1.99.1 release: {}",
                out.trim()
            ));
        }
    }

    prove_td_shell_userland(
        runner,
        &[&build_out, &link_out],
        &tools,
        rust_base,
        link_base,
        glibc_base,
    )?;
    println!(
        "PASS: rust-toolchain: upstream Rust 1.99.1 rustc and Cargo run on td glibc under own-root /td/store, and td shell builds (or reuses from its cache) and runs ripgrep/fd/uutils there with /gnu/store absent"
    );
    Ok(())
}

/// `stem`'s multicall at `/td/store/<base>/bin/<stem>`, built by its plan
/// into the check's store, which the proof's namespace binds.
fn script_tool(runner: &RecipeCheckRunner, stem: &str) -> Result<String, String> {
    runner.prepare_recipe_target(stem)?;
    let out = runner.build_plan(stem)?;
    let tree = runner.ladder_out_from(&out, stem)?;
    let binary = tree.join("bin").join(stem);
    if !is_executable(&binary) {
        return Err(format!(
            "{stem}: bin/{stem} is not an executable in the check's store ({})",
            binary.display()
        ));
    }
    Ok(format!(
        "{TD_STORE_DIR}/{}/bin/{stem}",
        path_basename(&tree)?
    ))
}

/// The packages `td shell` builds: the runner's declared builds, which
/// must be `PROVED_USERLAND` then `SCRIPT_USERLAND`, so the key and reach
/// that read them name exactly what is built here.
fn shell_packages<'a>(declared: &'a [&'a str]) -> Result<&'a [&'a str], String> {
    match declared.split_at_checked(PROVED_USERLAND.len()) {
        Some((proved, tools)) if proved == PROVED_USERLAND && tools == SCRIPT_USERLAND => {
            Ok(proved)
        }
        _ => Err(format!(
            "rust-toolchain declares the builds [{}] but its product proof exercises \
             [{}] under [{}]; change both together",
            declared.join(" "),
            PROVED_USERLAND.join(" "),
            SCRIPT_USERLAND.join(" ")
        )),
    }
}

fn prove_td_shell_userland(
    runner: &RecipeCheckRunner,
    build_outs: &[&Path],
    tools: &[String],
    rust_base: &str,
    link_base: &str,
    glibc_base: &str,
) -> Result<(), String> {
    let root = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let vendor_root = root.join(".td-build-cache/crate-vendor");
    // Their versions come from the recipes, so a bump needs no edit to
    // this proof.
    let packages = shell_packages(CheckRunner::RustToolchain.extra_builds())?;
    let (sh, util, txt) = match tools {
        [sh, util, txt] => (sh, util, txt),
        _ => {
            return Err(format!(
                "the product proof needs [{}], got {} tools",
                SCRIPT_USERLAND.join(" "),
                tools.len()
            ))
        }
    };
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

    // The toolchain's and td-link's plans each vouch their own outputs.
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
    // td-sh runs it with no PATH to search: every program is named by its
    // store path, the multicalls by their applet argument. One build of
    // each product may be visible, and it is the one td shell made, so its
    // companion, assembly-exception marker and freedom from /gnu/store are
    // checked here; rust-toolchain-test grades the plan builds' ELF.
    let script = format!(
        "set -eu\n\
         test ! -e /gnu/store\n\
         PATH=/nonexistent\n\
         export PATH\n\
         util='{util}'\n\
         txt='{txt}'\n\
         rg=''\n\
         for p in {TD_STORE_DIR}/*-ripgrep-{rg_version}/bin/rg; do\n\
           if test -x \"$p\"; then test -z \"$rg\" || exit 89; rg=$p; fi\n\
         done\n\
         fd=''\n\
         for p in {TD_STORE_DIR}/*-fd-{fd_version}/bin/fd; do\n\
           if test -x \"$p\"; then test -z \"$fd\" || exit 89; fd=$p; fi\n\
         done\n\
         uutils=''\n\
         for p in {TD_STORE_DIR}/*-uutils-{uutils_version}/bin/coreutils; do\n\
           if test -x \"$p\"; then test -z \"$uutils\" || exit 89; uutils=$p; fi\n\
         done\n\
         test -n \"$rg\"\n\
         test -n \"$fd\"\n\
         test -n \"$uutils\"\n\
         for b in \"$rg\" \"$fd\" \"$uutils\"; do\n\
           package=${{b%/bin/*}}\n\
           test -f \"$package/lib/debug/bin/${{b##*/}}.debug\" || exit 93\n\
           for line in exception.0.source=glibc-x86-64 exception.1.source=gcc-runtime exception.2.source=rust-toolchain; do\n\
             \"$txt\" grep -F -x \"$line\" \"$package/lib/debug/.td-assembly-exception\" >/dev/null || exit 94\n\
           done\n\
           rc=0; \"$txt\" grep -a -F -q /gnu/store \"$b\" || rc=$?; [ \"$rc\" = 1 ] || exit 90\n\
         done\n\
         \"$util\" mkdir -p /tmp/td-shell-userland/sub\n\
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
         rc=0; \"$txt\" grep -F -x stdbuf /tmp/td-shell-userland/uutils-list >/dev/null || rc=$?; [ \"$rc\" = 1 ] || exit 100\n\
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
    // Every Cargo recipe builds in the Debian build root.
    let (root_store, seed_db) = runner.build_root_seed()?;
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
        .env("TD_SHELL_NATIVE_BUILD_ROOT_STORE", path_str(root_store)?)
        .env("TD_SHELL_NATIVE_BUILD_ROOT_SEED_DB", path_str(seed_db)?)
        .env("TD_SHELL_NATIVE_LOCK", lock_s)
        .env("TD_PERSIST_STORE", tdstore_s)
        .env("TD_PERSIST_DB", persist_db_s);
    // The shell asks this evaluator for each recipe it builds; the command
    // carries the check's confinement, so it refuses any not declared.
    cmd.arg("shell")
        .args(packages)
        .args(["--", store_ns_builder_s])
        .arg("store-ns")
        .arg(tdstore_s)
        .args(["--", sh.as_str(), "-c", &script]);
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
        let declared = td_recipe::types::CheckRunner::RustToolchain.extra_builds();
        let mut proved = super::PROVED_USERLAND.to_vec();
        proved.extend_from_slice(super::SCRIPT_USERLAND);
        assert_eq!(proved, declared);
        assert_eq!(
            super::shell_packages(declared).unwrap(),
            super::PROVED_USERLAND
        );
        // A drift either way is refused, not built.
        for drifted in [
            &["ripgrep", "fd", "uutils"][..],
            &["ripgrep", "fd", "uutils", "td-sh", "td-txt", "td-util"],
            &[
                "ripgrep", "fd", "uutils", "td-sh", "td-util", "td-txt", "gawk",
            ],
        ] {
            let refused = super::shell_packages(drifted).unwrap_err();
            assert!(refused.contains("change both together"), "{refused}");
        }
        // No ladder rung serves the proof any more.
        for stem in declared {
            assert!(
                !stem.contains("mesboot") && !stem.ends_with("-self"),
                "{stem}"
            );
        }
    }
}
