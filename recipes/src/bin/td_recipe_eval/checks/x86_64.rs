use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check_runner::{
    command_ok, command_output, copy_tree, extract_line_value, find_first_named, is_executable,
    linux_version_from_file, path_str, read_dir_sorted, reject_embedded_gnu_store,
    remove_path_if_exists, require_exec, require_file, require_output_line, set_executable,
    sha256_file, source_pin_for_key, validate_source_file_basename, verify_source_pin,
    RecipeCheckRunner, TD_STORE_DIR,
};

const GCC_X8664_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64";
const GCC_X8664_NATIVE_STAGE: &str = "stage/td/store/gcc-14.3.0-x86_64-native";
const GLIBC_X8664_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

pub(crate) fn run_cross_toolchain(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("gcc-x86-64-stage2")?;
    let build_out = runner.build_plan("gcc-x86-64-stage2")?;
    let xbu =
        runner.stage_tree_under_tdstore(&runner.ladder_out_from(&build_out, "binutils-x86-64")?)?;
    let xgcc2 = runner.stage_tree_under_tdstore(
        &runner
            .ladder_out_from(&build_out, "gcc-x86-64-stage2")?
            .join(GCC_X8664_STAGE),
    )?;
    let xglibc = runner.stage_tree_under_tdstore(
        &runner
            .ladder_out_from(&build_out, "glibc-x86-64")?
            .join(GLIBC_X8664_STAGE),
    )?;
    verify_cross_outputs(runner, &xbu, &xgcc2, &xglibc)?;
    println!(
        "PASS: gcc-x86-64-stage2 - recipe graph built the x86_64 cross toolchain and its dynamic C/C++ outputs run in td's own /td/store root"
    );
    Ok(())
}

pub(crate) fn run_native_gcc(runner: &RecipeCheckRunner) -> Result<(), String> {
    let (xnbu, xngcc, xglibc) = build_native_recipe_outputs(runner)?;
    verify_native_outputs(runner, &xnbu, &xngcc, &xglibc, "gcc-14.3.0-x86_64-native")?;
    println!(
        "PASS: gcc-x86-64-native - recipe graph built an ELF64 native x86_64 gcc that compiles and runs C/C++ outputs in td's own /td/store root"
    );
    Ok(())
}

pub(crate) fn run_self_gcc(runner: &RecipeCheckRunner) -> Result<(), String> {
    let (xnbu, xngcc, xglibc) = build_native_recipe_outputs(runner)?;
    let self_out = runner.scratch().join("x86_64-self-gcc");
    remove_path_if_exists(&self_out)?;
    fs::create_dir_all(&self_out).map_err(|e| format!("mkdir {}: {e}", self_out.display()))?;
    let cpath = curated_path(runner)?;
    let mut cmd = runner.builder_command();
    cmd.arg("toolchain-recipe")
        .arg("x86_64-self")
        .env("TDXS_CPATH", &cpath)
        .env("TDXS_BUILDER_GCC", &xngcc)
        .env("TDXS_BUILDER_BINUTILS", &xnbu)
        .env("TDXS_GLIBC", &xglibc)
        .env(
            "TDXS_BINUTILS_TAR",
            source_file_for_key(runner, "binutils-244-source")?,
        )
        .env(
            "TDXS_GCC_TAR",
            source_file_for_key(runner, "gcc-14-source")?,
        )
        .env("TDXS_GMP_TAR", source_file_for_key(runner, "gmp63")?)
        .env("TDXS_MPFR_TAR", source_file_for_key(runner, "mpfr421")?)
        .env("TDXS_MPC_TAR", source_file_for_key(runner, "mpc131")?)
        .env(
            "TDXS_KERNEL_HEADERS_TAR",
            linux_headers_file(runner, "x86_64")?,
        )
        .env("TDXS_OUT", &self_out)
        .env(
            "X86_MAKE_J",
            env::var("X86_MAKE_J").unwrap_or_else(|_| "-j4".to_string()),
        );
    let log = command_output(&mut cmd, "td-builder toolchain-recipe x86_64-self")?;
    io::stdout()
        .write_all(log.as_bytes())
        .map_err(|e| format!("write self-gcc log: {e}"))?;
    let xsbu = extract_line_value(&log, "SELF_BINUTILS=")
        .map(PathBuf::from)
        .ok_or_else(|| "toolchain-recipe x86_64-self returned no SELF_BINUTILS".to_string())?;
    let xsgcc = extract_line_value(&log, "SELF_GCC=")
        .map(PathBuf::from)
        .ok_or_else(|| "toolchain-recipe x86_64-self returned no SELF_GCC".to_string())?;
    assert_codegen_agreement(runner, &xngcc, &xsgcc)?;
    let staged_sbu = runner.stage_tree_under_tdstore(&xsbu)?;
    let staged_sgcc = runner.stage_tree_under_tdstore(&xsgcc)?;
    verify_native_outputs(
        runner,
        &staged_sbu,
        &staged_sgcc,
        &xglibc,
        "gcc-14.3.0-x86_64-self",
    )?;
    println!(
        "PASS: gcc-x86-64-native self-host - native recipe output rebuilt gcc and the rebuilt compiler agrees on codegen and runs in td's own /td/store root"
    );
    Ok(())
}

fn build_native_recipe_outputs(
    runner: &RecipeCheckRunner,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    runner.prepare_recipe_target("gcc-x86-64-native")?;
    let build_out = runner.build_plan("gcc-x86-64-native")?;
    let xnbu = runner
        .stage_tree_under_tdstore(&runner.ladder_out_from(&build_out, "binutils-x86-64-native")?)?;
    let xngcc = runner.stage_tree_under_tdstore(
        &runner
            .ladder_out_from(&build_out, "gcc-x86-64-native")?
            .join(GCC_X8664_NATIVE_STAGE),
    )?;
    let xglibc = runner.stage_tree_under_tdstore(
        &runner
            .ladder_out_from(&build_out, "glibc-x86-64")?
            .join(GLIBC_X8664_STAGE),
    )?;
    Ok((xnbu, xngcc, xglibc))
}

fn verify_cross_outputs(
    runner: &RecipeCheckRunner,
    xbu: &Path,
    xgcc: &Path,
    xglibc: &Path,
) -> Result<(), String> {
    let readelf = xbu.join("bin/x86_64-pc-linux-gnu-readelf");
    require_exec(&readelf, "cross readelf")?;
    require_exec(&xgcc.join("bin/x86_64-pc-linux-gnu-gcc"), "cross gcc")?;
    require_exec(&xgcc.join("bin/x86_64-pc-linux-gnu-g++"), "cross g++")?;
    require_file(&xglibc.join("lib/libc.so.6"), "x86_64 libc")?;
    for path in [
        xglibc.join("lib/libc.so.6"),
        xgcc.join("bin/x86_64-pc-linux-gnu-gcc"),
        find_first_named(xgcc, "cc1")?,
    ] {
        reject_embedded_gnu_store(&path)?;
    }
    let work = runner.fresh_scratch("x86-cross-probe")?;
    write_probe_sources(&work)?;
    let cpath = curated_path(runner)?;
    let glibc_logical = runner.logical_tdstore_path(xglibc)?;
    compile_cross(
        xbu,
        xgcc,
        xglibc,
        &glibc_logical,
        &cpath,
        &work,
        "gcc",
        "c.c",
        "c.out",
    )?;
    compile_cross(
        xbu,
        xgcc,
        xglibc,
        &glibc_logical,
        &cpath,
        &work,
        "g++",
        "cpp.cc",
        "cpp.out",
    )?;
    assert_elf64_x86_64(&readelf, &work.join("c.out"), "cross C probe")?;
    assert_interp(&readelf, &work.join("c.out"), &glibc_logical)?;
    reject_embedded_gnu_store(&work.join("c.out"))?;
    reject_embedded_gnu_store(&work.join("cpp.out"))?;
    stage_and_run_probes(runner, &work, "x86-cross-probe")?;
    Ok(())
}

fn verify_native_outputs(
    runner: &RecipeCheckRunner,
    xnbu: &Path,
    xngcc: &Path,
    xglibc: &Path,
    expected_gcc_name: &str,
) -> Result<(), String> {
    let readelf = xnbu.join("bin/readelf");
    require_exec(&readelf, "native readelf")?;
    require_exec(&xngcc.join("bin/gcc"), "native gcc")?;
    require_exec(&xngcc.join("bin/g++"), "native g++")?;
    require_exec(&xnbu.join("bin/as"), "native as")?;
    require_exec(&xnbu.join("bin/ld"), "native ld")?;
    require_file(&xglibc.join("lib/libc.so.6"), "x86_64 libc")?;
    assert_elf64_x86_64(&readelf, &xngcc.join("bin/gcc"), expected_gcc_name)?;
    for path in [
        xngcc.join("bin/gcc"),
        find_first_named(xngcc, "cc1")?,
        xnbu.join("bin/as"),
        xnbu.join("bin/ld"),
        xglibc.join("lib/libc.so.6"),
    ] {
        reject_embedded_gnu_store(&path)?;
    }
    let work = runner.fresh_scratch("x86-native-probe")?;
    write_probe_sources(&work)?;
    let cpath = curated_path(runner)?;
    let glibc_logical = runner.logical_tdstore_path(xglibc)?;
    let nbu_logical = runner.logical_tdstore_path(xnbu)?;
    let ngcc_logical = runner.logical_tdstore_path(xngcc)?;
    compile_native_in_ownroot(
        runner,
        &ngcc_logical,
        &nbu_logical,
        &glibc_logical,
        "gcc",
        "c",
        c_probe_source(),
    )?;
    compile_native_in_ownroot(
        runner,
        &ngcc_logical,
        &nbu_logical,
        &glibc_logical,
        "g++",
        "cpp",
        cpp_probe_source(),
    )?;
    let work = runner.fresh_scratch("x86-native-host-probe")?;
    write_probe_sources(&work)?;
    compile_native_host(
        xnbu,
        xngcc,
        xglibc,
        &glibc_logical,
        &cpath,
        &work,
        "gcc",
        "c.c",
        "c.out",
    )?;
    compile_native_host(
        xnbu,
        xngcc,
        xglibc,
        &glibc_logical,
        &cpath,
        &work,
        "g++",
        "cpp.cc",
        "cpp.out",
    )?;
    assert_elf64_x86_64(&readelf, &work.join("c.out"), "native C probe")?;
    assert_interp(&readelf, &work.join("c.out"), &glibc_logical)?;
    reject_embedded_gnu_store(&work.join("c.out"))?;
    reject_embedded_gnu_store(&work.join("cpp.out"))?;
    Ok(())
}

fn assert_codegen_agreement(
    runner: &RecipeCheckRunner,
    native_gcc: &Path,
    self_gcc: &Path,
) -> Result<(), String> {
    let work = runner.fresh_scratch("x86-self-codegen")?;
    fs::write(
        work.join("cg.c"),
        "unsigned fib(unsigned n) { unsigned a = 0, b = 1; while (n--) { unsigned t = a + b; a = b; b = t; } return a; }\n\
         int classify(int x) { switch (x & 3) { case 0: return x / 3; case 1: return x * 5; case 2: return x - 7; default: return -x; } }\n\
         int main(void) { return (fib(12) == 144 && classify(9) == 45) ? 42 : 1; }\n",
    )
    .map_err(|e| format!("write codegen C source: {e}"))?;
    fs::write(
        work.join("cg.cc"),
        "template <typename T> struct Acc { T v; explicit Acc(T s) : v(s) {} Acc &add(T x) { v += x; return *this; } };\n\
         template <typename T> T sq(T x) { return x * x; }\n\
         int main() { Acc<int> a(2); a.add(sq(3)).add(sq(5)); return a.v == 36 ? 42 : 1; }\n",
    )
    .map_err(|e| format!("write codegen C++ source: {e}"))?;
    for (tree, prefix) in [(native_gcc, "native"), (self_gcc, "self")] {
        require_exec(&tree.join("bin/gcc"), "codegen gcc")?;
        require_exec(&tree.join("bin/g++"), "codegen g++")?;
        compile_to_assembly(tree, "gcc", &work, "cg.c", &format!("{prefix}-c.s"))?;
        compile_to_assembly(tree, "g++", &work, "cg.cc", &format!("{prefix}-cpp.s"))?;
    }
    for (label, a, b) in [
        ("c", "native-c.s", "self-c.s"),
        ("cpp", "native-cpp.s", "self-cpp.s"),
    ] {
        let ha = sha256_file(&work.join(a))?;
        let hb = sha256_file(&work.join(b))?;
        if ha != hb {
            return Err(format!(
                "{label} assembly differs between native gcc ({ha}) and self-rebuilt gcc ({hb})"
            ));
        }
    }
    Ok(())
}

fn stage_and_run_probes(runner: &RecipeCheckRunner, work: &Path, name: &str) -> Result<(), String> {
    let root = runner.tdstore_path().join(name);
    remove_path_if_exists(&root)?;
    fs::create_dir_all(root.join("bin"))
        .map_err(|e| format!("mkdir {}: {e}", root.join("bin").display()))?;
    for (src, dst) in [("c.out", "c"), ("cpp.out", "cpp")] {
        let to = root.join("bin").join(dst);
        fs::copy(work.join(src), &to)
            .map_err(|e| format!("copy {src} to {}: {e}", to.display()))?;
        set_executable(&to)?;
    }
    let c = format!("{TD_STORE_DIR}/{name}/bin/c");
    let c_out = runner.store_ns_output(&[c.as_str()], None)?;
    require_output_line(&c_out, "C-RAN", "cross C probe did not run")?;
    require_output_line(
        &c_out,
        "GNU-ABSENT",
        "/gnu/store is present in cross C probe root",
    )?;
    let cpp = format!("{TD_STORE_DIR}/{name}/bin/cpp");
    let cpp_out = runner.store_ns_output(&[cpp.as_str()], None)?;
    require_output_line(&cpp_out, "CPP-RAN", "cross C++ probe did not run")?;
    require_output_line(
        &cpp_out,
        "GNU-ABSENT",
        "/gnu/store is present in cross C++ probe root",
    )
}

fn stage_static_bash(runner: &RecipeCheckRunner) -> Result<String, String> {
    let src = match env::var_os("TD_GATE_INPUT_BASH_STATIC").map(PathBuf::from) {
        Some(path) if is_executable(&path.join("bin/bash")) => path,
        _ => {
            let lock_rel = "tests/td-subst.lock";
            let lock_text = fs::read_to_string(runner.root().join(lock_rel))
                .map_err(|e| format!("read {lock_rel}: {e}"))?;
            let bash = lock_text
                .lines()
                .find(|line| line.contains("-bash-") && !line.contains("static"))
                .and_then(|line| line.split_once(' ').map(|(_, path)| path.trim()))
                .ok_or_else(|| format!("no dynamic bash entry in {lock_rel}"))?;
            let mut cmd = runner.builder_command();
            cmd.arg("store-closure-scan").arg("/gnu/store").arg(bash);
            let scan = command_output(&mut cmd, "store-closure-scan bash")?;
            scan.lines()
                .find(|line| line.contains("-bash-static-"))
                .map(|line| PathBuf::from(line.trim()))
                .ok_or_else(|| {
                    format!("no bash-static member in the /gnu/store closure of {bash}")
                })?
        }
    };
    require_exec(&src.join("bin/bash"), "static bash")?;
    let base = src
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("static bash path has no UTF-8 basename: {}", src.display()))?;
    let dst = runner.tdstore_path().join(base);
    if !is_executable(&dst.join("bin/bash")) {
        remove_path_if_exists(&dst)?;
        copy_tree(&src, &dst).map_err(|e| {
            format!(
                "stage static bash into tdstore failed ({} -> {}): {e}",
                src.display(),
                dst.display()
            )
        })?;
    }
    Ok(base.to_string())
}

fn store_ns_bash(runner: &RecipeCheckRunner, script: &str) -> Result<String, String> {
    let bash_base = stage_static_bash(runner)?;
    let bash = format!("{TD_STORE_DIR}/{bash_base}/bin/bash");
    runner.store_ns_output(&[bash.as_str(), "-c", script], None)
}

fn source_file_for_key(runner: &RecipeCheckRunner, key: &str) -> Result<PathBuf, String> {
    let pin = source_pin_for_key(key)?;
    validate_source_file_basename(&pin)?;
    let file = runner
        .root()
        .join(".td-build-cache/sources")
        .join(&pin.file);
    if !file.is_file() {
        return Err(format!(
            "pinned source not warm for {key}: {}",
            file.display()
        ));
    }
    verify_source_pin(&file, &pin)?;
    Ok(file)
}

fn linux_headers_file(runner: &RecipeCheckRunner, arch: &str) -> Result<PathBuf, String> {
    let pin = source_pin_for_key("linux-source")?;
    let version = linux_version_from_file(&pin.file)?;
    let file = runner
        .root()
        .join(".td-build-cache/sources")
        .join(format!("linux-headers-{version}-{arch}.tar.gz"));
    if !file.is_file() {
        return Err(format!("kernel headers not warm: {}", file.display()));
    }
    Ok(file)
}

fn write_probe_sources(work: &Path) -> Result<(), String> {
    fs::write(work.join("c.c"), c_probe_source()).map_err(|e| format!("write C probe: {e}"))?;
    fs::write(work.join("cpp.cc"), cpp_probe_source())
        .map_err(|e| format!("write C++ probe: {e}"))?;
    Ok(())
}

fn c_probe_source() -> &'static str {
    "#include <stdio.h>\n#include <unistd.h>\nint main(void) { puts(\"C-RAN\"); puts(access(\"/gnu/store\", F_OK) == 0 ? \"GNU-PRESENT\" : \"GNU-ABSENT\"); return 0; }\n"
}

fn cpp_probe_source() -> &'static str {
    "#include <iostream>\n#include <unistd.h>\n#include <vector>\nint main() { std::vector<int> v; for (int i = 0; i < 43; ++i) v.push_back(i); if (v[42] != 42) return 1; std::cout << \"CPP-RAN\\n\" << (access(\"/gnu/store\", F_OK) == 0 ? \"GNU-PRESENT\\n\" : \"GNU-ABSENT\\n\"); return 0; }\n"
}

fn compile_cross(
    xbu: &Path,
    xgcc: &Path,
    xglibc: &Path,
    glibc_logical: &str,
    cpath: &str,
    work: &Path,
    compiler: &str,
    src: &str,
    out: &str,
) -> Result<(), String> {
    let bin_name = match compiler {
        "gcc" => "x86_64-pc-linux-gnu-gcc",
        "g++" => "x86_64-pc-linux-gnu-g++",
        other => return Err(format!("unknown x86_64 cross compiler `{other}'")),
    };
    let mut cmd = Command::new(xgcc.join("bin").join(bin_name));
    cmd.current_dir(work)
        .env("PATH", path_with_tool_dir(&xbu.join("bin"), cpath)?)
        .arg("-isystem")
        .arg(xglibc.join("include"))
        .arg(format!("-B{}", xglibc.join("lib").display()))
        .arg(format!("-L{}", xglibc.join("lib").display()))
        .arg("-static-libgcc");
    if compiler == "g++" {
        cmd.arg("-static-libstdc++");
    }
    cmd.arg("-Wl,--dynamic-linker")
        .arg(format!("-Wl,{glibc_logical}/lib/ld-linux-x86-64.so.2"))
        .arg("-Wl,--enable-new-dtags")
        .arg("-Wl,-rpath")
        .arg(format!("-Wl,{glibc_logical}/lib"))
        .arg("-o")
        .arg(out)
        .arg(src);
    command_ok(&mut cmd, &format!("x86_64 cross {compiler} compile"))
}

fn compile_native_host(
    xnbu: &Path,
    xngcc: &Path,
    xglibc: &Path,
    glibc_logical: &str,
    cpath: &str,
    work: &Path,
    compiler: &str,
    src: &str,
    out: &str,
) -> Result<(), String> {
    let bin_name = match compiler {
        "gcc" | "g++" => compiler,
        other => return Err(format!("unknown x86_64 native compiler `{other}'")),
    };
    let mut cmd = Command::new(xngcc.join("bin").join(bin_name));
    cmd.current_dir(work)
        .env("PATH", path_with_tool_dir(&xnbu.join("bin"), cpath)?)
        .arg("-idirafter")
        .arg(xglibc.join("include"))
        .arg(format!("-B{}", xnbu.join("bin").display()))
        .arg(format!("-B{}", xglibc.join("lib").display()))
        .arg(format!("-L{}", xglibc.join("lib").display()))
        .arg("-static-libgcc");
    if compiler == "g++" {
        cmd.arg("-static-libstdc++");
    }
    cmd.arg("-Wl,--dynamic-linker")
        .arg(format!("-Wl,{glibc_logical}/lib/ld-linux-x86-64.so.2"))
        .arg("-Wl,--enable-new-dtags")
        .arg("-Wl,-rpath")
        .arg(format!("-Wl,{glibc_logical}/lib"))
        .arg("-o")
        .arg(out)
        .arg(src);
    command_ok(&mut cmd, &format!("x86_64 native host {compiler} compile"))
}

fn compile_native_in_ownroot(
    runner: &RecipeCheckRunner,
    ngcc_logical: &str,
    nbu_logical: &str,
    glibc_logical: &str,
    compiler: &str,
    out_stem: &str,
    source: &str,
) -> Result<(), String> {
    let (lang, src_name, run_marker, class_marker, machine_marker, interp_marker) =
        match (compiler, out_stem) {
            ("gcc", "c") => ("c", "probe.c", "C-RAN", "C-ELF64", "C-MACH", "C-INTERP"),
            ("g++", "cpp") => (
                "c++",
                "probe.cc",
                "CPP-RAN",
                "CPP-ELF64",
                "CPP-MACH",
                "CPP-INTERP",
            ),
            (other_compiler, other_stem) => {
                return Err(format!(
                    "unsupported native own-root probe {other_compiler}/{other_stem}"
                ))
            }
        };
    let stdcxx = if compiler == "g++" {
        " -static-libstdc++"
    } else {
        ""
    };
    let out_path = format!("/tmp/td-x86-{out_stem}");
    let script = format!(
        "set -eu\n\
         export PATH={ngcc_logical}/bin:{nbu_logical}/bin\n\
         cd /tmp\n\
         cat > {src_name} <<'TD_X86_PROBE'\n\
{source}\
TD_X86_PROBE\n\
         {ngcc_logical}/bin/{compiler} -x {lang} \
           -idirafter {glibc_logical}/include \
           -B{nbu_logical}/bin \
           -B{glibc_logical}/lib \
           -L{glibc_logical}/lib \
           -static-libgcc{stdcxx} \
           -Wl,--dynamic-linker \
           -Wl,{glibc_logical}/lib/ld-linux-x86-64.so.2 \
           -Wl,--enable-new-dtags \
           -Wl,-rpath \
           -Wl,{glibc_logical}/lib \
           -o {out_path} {src_name}\n\
         hdr=$({nbu_logical}/bin/readelf -h {out_path})\n\
         case \"$hdr\" in *ELF64*) echo {class_marker}=ELF64 ;; *) echo {class_marker}=BAD ;; esac\n\
         case \"$hdr\" in *X86-64*|*x86-64*) echo {machine_marker}=x86-64 ;; *) echo {machine_marker}=BAD ;; esac\n\
         phdr=$({nbu_logical}/bin/readelf -l {out_path})\n\
         case \"$phdr\" in *\"{glibc_logical}/lib/ld-linux-x86-64.so.2\"*) echo {interp_marker}=OK ;; *) echo {interp_marker}=BAD ;; esac\n\
         {out_path}\n\
         [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT\n"
    );
    let out = store_ns_bash(runner, &script)?;
    require_output_line(
        &out,
        &format!("{class_marker}=ELF64"),
        "native own-root compiler did not emit ELF64",
    )?;
    require_output_line(
        &out,
        &format!("{machine_marker}=x86-64"),
        "native own-root compiler did not emit x86-64",
    )?;
    require_output_line(
        &out,
        &format!("{interp_marker}=OK"),
        "native own-root compiler did not use the /td/store x86_64 loader",
    )?;
    require_output_line(&out, run_marker, "native own-root probe did not run")?;
    require_output_line(
        &out,
        "GNU-ABSENT",
        "/gnu/store is present in native own-root probe",
    )
}

fn assert_elf64_x86_64(readelf: &Path, bin: &Path, label: &str) -> Result<(), String> {
    let mut cmd = Command::new(readelf);
    cmd.arg("-h").arg(bin);
    let out = command_output(&mut cmd, &format!("readelf -h {label}"))?;
    if !out.contains("ELF64") {
        return Err(format!("{label} is not ELF64"));
    }
    if !(out.contains("X86-64") || out.contains("x86-64")) {
        return Err(format!("{label} is not x86-64"));
    }
    Ok(())
}

fn assert_interp(readelf: &Path, bin: &Path, glibc_logical: &str) -> Result<(), String> {
    let mut cmd = Command::new(readelf);
    cmd.arg("-l").arg(bin);
    let out = command_output(&mut cmd, "readelf -l probe")?;
    let expected = format!("{glibc_logical}/lib/ld-linux-x86-64.so.2");
    if out.contains(&expected) {
        Ok(())
    } else {
        Err(format!(
            "{} interp does not reference {expected}",
            bin.display()
        ))
    }
}

fn compile_to_assembly(
    gcc_tree: &Path,
    compiler: &str,
    work: &Path,
    src: &str,
    out: &str,
) -> Result<(), String> {
    let bin = match compiler {
        "gcc" | "g++" => gcc_tree.join("bin").join(compiler),
        other => return Err(format!("unknown codegen compiler `{other}'")),
    };
    let mut cmd = Command::new(bin);
    cmd.current_dir(work)
        .arg("-O2")
        .arg("-S")
        .arg("-frandom-seed=tdselfcodegen")
        .arg("-o")
        .arg(out)
        .arg(src);
    command_ok(&mut cmd, &format!("codegen {compiler}"))
}

fn curated_path(runner: &RecipeCheckRunner) -> Result<String, String> {
    let dir = runner.fresh_scratch("curated-bin")?;
    if let Some(paths) = env::var_os("PATH") {
        for path in env::split_paths(&paths) {
            if !path.is_dir() {
                continue;
            }
            for entry in read_dir_sorted(&path)? {
                let Some(name) = entry
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                if is_bad_build_path_tool(&name) {
                    continue;
                }
                let link = dir.join(&name);
                if link.exists() {
                    continue;
                }
                let _ = symlink(&entry, link);
            }
        }
    }
    path_str(&dir).map(str::to_string)
}

fn is_bad_build_path_tool(name: &str) -> bool {
    matches!(
        name,
        "as" | "ld" | "gcc" | "g++" | "cc" | "c++" | "cpp" | "ar" | "ranlib"
    ) || name.starts_with("gcc-")
        || name.starts_with("g++-")
        || name.starts_with("clang")
        || name.starts_with("guile")
        || name.starts_with("guix")
        || name.starts_with("tcc")
}

fn path_with_tool_dir(dir: &Path, cpath: &str) -> Result<String, String> {
    let dir_s = path_str(dir)?;
    if cpath.is_empty() {
        return Ok(dir_s.to_string());
    }
    Ok(format!("{dir_s}:{cpath}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_path_filter_rejects_versioned_host_tool_names() {
        for name in [
            "as",
            "ld",
            "gcc",
            "gcc-14",
            "g++",
            "g++-14",
            "clang",
            "clang++",
            "guile",
            "guile-3.0",
            "guix",
            "guix-daemon",
            "tcc",
        ] {
            assert!(is_bad_build_path_tool(name), "{name}");
        }
        for name in ["sed", "grep", "tar", "gzip", "makeinfo"] {
            assert!(!is_bad_build_path_tool(name), "{name}");
        }
    }
}
