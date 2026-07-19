use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check_runner::{is_executable, RecipeCheckRunner, TD_STORE_DIR};
use crate::sha256::Sha256;

const GCC_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64-self";
const GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("rust-toolchain")?;
    let build_out = runner.build_plan("rust-toolchain")?;
    let rust_tree = runner.ladder_out_from(&build_out, "rust-toolchain")?;
    let stage0_tree = runner.ladder_out_from(&build_out, "rust-stage0")?;
    let gcc_tree = runner.ladder_out_from(&build_out, "gcc-x86-64-self")?;
    let binutils_tree = runner.ladder_out_from(&build_out, "binutils-x86-64-self")?;
    let glibc_tree = runner.ladder_out_from(&build_out, "glibc-x86-64")?;
    let busybox_tree = runner.ladder_out_from(&build_out, "busybox-x86-64")?;
    println!(
        "   [ladder] x86_64 Rust bridge via build-plan --auto: exact stage0 snapshot -> source-built rustc/std/Cargo ({})",
        rust_tree.display()
    );
    for binary in ["rustc", "rustdoc", "cargo"] {
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
    let busybox_base = path_basename(&busybox_tree)?;
    let rust_path = format!("{TD_STORE_DIR}/{rust_base}");
    let gcc_path = format!("{TD_STORE_DIR}/{gcc_base}/{GCC_STAGE}");
    let binutils_path = format!("{TD_STORE_DIR}/{binutils_base}/bin");
    let glibc_path = format!("{TD_STORE_DIR}/{glibc_base}/{GLIBC_STAGE}");
    let busybox_path = format!("{TD_STORE_DIR}/{busybox_base}/bin/busybox");

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
         '{busybox_path}' mkdir -p /tmp/td-cargo-smoke/src /tmp/td-cargo-home /tmp/td-cargo-target\n\
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
    let smoke_out = runner.store_ns_output(&[&busybox_path, "sh", "-c", &smoke], None)?;
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
        &busybox_path,
        rust_base,
        gcc_base,
        binutils_base,
        glibc_base,
        busybox_base,
    )?;
    println!(
        "PASS: rust-toolchain: source-built Rust 1.96.0 rustc/std/Cargo contain no stage0 artifacts; td shell builds and runs ripgrep/fd/uutils against td GCC/glibc with /gnu/store absent"
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
    busybox_path: &str,
    rust_base: &str,
    gcc_base: &str,
    binutils_base: &str,
    glibc_base: &str,
    busybox_base: &str,
) -> Result<(), String> {
    let root = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let vendor_root = root.join(".td-build-cache/crate-vendor");
    for package in ["ripgrep", "fd", "uutils"] {
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
         glibc-x86-64 {TD_STORE_DIR}/{glibc_base} td-recipe-output\n\
         busybox-x86-64 {TD_STORE_DIR}/{busybox_base} td-recipe-output\n"
    );
    fs::write(&native_lock, lock)
        .map_err(|e| format!("write {}: {e}", native_lock.display()))?;

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
    let evaluator = std::env::current_exe()
        .map_err(|e| format!("locate td-recipe-eval: {e}"))?;
    let stage0_base = path_basename(stage0_tree)?;
    let interp = format!("{glibc_path}/lib/ld-linux-x86-64.so.2");
    let script = format!(
        "set -eu\n\
         test ! -e /gnu/store\n\
         export PATH='{TD_STORE_DIR}/{busybox_base}/bin'\n\
         rg=''\n\
         for p in {TD_STORE_DIR}/*-ripgrep-14.1.1/bin/rg; do\n\
           if test -x \"$p\"; then rg=$p; fi\n\
         done\n\
         fd=''\n\
         for p in {TD_STORE_DIR}/*-fd-10.2.0/bin/fd; do\n\
           if test -x \"$p\"; then fd=$p; fi\n\
         done\n\
         uutils=''\n\
         for p in {TD_STORE_DIR}/*-uutils-0.9.0/bin/coreutils; do\n\
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
         if \"$uutils\" --list | grep -F -x stdbuf >/dev/null; then exit 99; fi\n\
         readelf='{binutils_path}/readelf'\n\
         \"$readelf\" -l \"$rg\" | grep -F '{interp}' >/dev/null\n\
         \"$readelf\" -l \"$fd\" | grep -F '{interp}' >/dev/null\n\
         \"$readelf\" -l \"$uutils\" | grep -F '{interp}' >/dev/null\n\
         for binary in \"$rg\" \"$fd\" \"$uutils\"; do\n\
           if grep -a -F /gnu/store \"$binary\" >/dev/null; then exit 93; fi\n\
           if grep -a -F '{stage0_base}' \"$binary\" >/dev/null; then exit 94; fi\n\
         done\n\
         printf '%s\\n' TD-SHELL-USERLAND-OK\n"
    );

    let tdstore_s = path_str(&tdstore)?;
    let product_s = path_str(&product)?;
    let tmp = product.join("tmp");
    let tmp_s = path_str(&tmp)?;
    let vendor_s = path_str(&vendor_root)?;
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
        .env("TD_SHELL_CACHE", product.join("packages"))
        .env("TD_SHELL_VENDOR_ROOT", vendor_s)
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
        .env("TD_PERSIST_DB", persist_db_s)
        .args(["shell", "ripgrep", "fd", "uutils", "--", store_ns_builder_s])
        .arg("store-ns")
        .arg(tdstore_s)
        .args(["--", busybox_path, "sh", "-c", &script]);
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
        "   [product] td shell built ripgrep 14.1.1, fd 10.2.0, and uutils 0.9.0 with source-built stage2 and ran all three under own-root /td/store"
    );
    Ok(())
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

fn path_basename(path: &Path) -> Result<&str, String> {
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
/// artifacts are large enough that reading each once keeps the daily trust-root
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
}
