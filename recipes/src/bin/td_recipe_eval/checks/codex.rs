use crate::check_runner::{is_executable, RecipeCheckRunner, TD_STORE_DIR};
use super::rust_toolchain::{path_basename, GLIBC_STAGE};
use td_recipe::ladder::CODEX_VERSION_OUTPUT;

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("codex")?;
    let build_out = runner.build_plan("codex")?;
    let codex_tree = runner.ladder_out_from(&build_out, "codex")?;
    let glibc_tree = runner.ladder_out_from(&build_out, "glibc-x86-64")?;
    let binutils_tree = runner.ladder_out_from(&build_out, "binutils-x86-64-self")?;
    let busybox_tree = runner.ladder_out_from(&build_out, "busybox-x86-64")?;

    let binary = codex_tree.join("bin/codex");
    if !is_executable(&binary) {
        return Err(format!("Codex executable is absent: {}", binary.display()));
    }
    let debug = codex_tree.join("lib/debug/bin/codex.debug");
    if !debug.is_file() {
        return Err(format!(
            "Codex debug companion is absent: {}",
            debug.display()
        ));
    }

    let codex_base = path_basename(&codex_tree)?;
    let glibc_base = path_basename(&glibc_tree)?;
    let binutils_base = path_basename(&binutils_tree)?;
    let busybox_base = path_basename(&busybox_tree)?;
    let codex = format!("{TD_STORE_DIR}/{codex_base}/bin/codex");
    let debug = format!("{TD_STORE_DIR}/{codex_base}/lib/debug/bin/codex.debug");
    let marker = format!("{TD_STORE_DIR}/{codex_base}/lib/debug/.td-assembly-exception");
    let line_marker =
        format!("{TD_STORE_DIR}/{codex_base}/lib/debug/.td-line-attribution-exception");
    let readelf = format!("{TD_STORE_DIR}/{binutils_base}/bin/readelf");
    let nm = format!("{TD_STORE_DIR}/{binutils_base}/bin/nm");
    let busybox = format!("{TD_STORE_DIR}/{busybox_base}/bin/busybox");
    let interpreter = format!("{TD_STORE_DIR}/{glibc_base}/{GLIBC_STAGE}/lib/ld-linux-x86-64.so.2");

    let version = runner.store_ns_output(&[&codex, "--version"], None)?;
    if version.trim() != CODEX_VERSION_OUTPUT {
        return Err(format!("unexpected Codex version: {}", version.trim()));
    }
    let help = runner.store_ns_output(&[&codex, "--help"], None)?;
    for required in [
        "Codex CLI",
        "exec",
        "review",
        "login",
        "mcp-server",
        "sandbox",
    ] {
        if !help.contains(required) {
            return Err(format!(
                "Codex help omits daily-driver surface {required:?}"
            ));
        }
    }

    let smoke = format!(
        "set -eu\n\
         test ! -e /gnu/store\n\
         runtime_id=\"$(\"{readelf}\" -n '{codex}' | \"{busybox}\" grep 'Build ID:')\"\n\
         debug_id=\"$(\"{readelf}\" -n '{debug}' | \"{busybox}\" grep 'Build ID:')\"\n\
         test -n \"$runtime_id\"\n\
         test \"$runtime_id\" = \"$debug_id\"\n\
         if \"{readelf}\" -S '{codex}' | \"{busybox}\" grep -F '.symtab' >/dev/null; then exit 81; fi\n\
         \"{readelf}\" -S '{debug}' | \"{busybox}\" grep -F '.symtab' >/dev/null\n\
         \"{readelf}\" -S '{debug}' | \"{busybox}\" grep -F '.debug_line' >/dev/null\n\
         test \"$(\"{readelf}\" -SW '{debug}' | \"{busybox}\" awk '{{ for (i = 1; i <= NF; i++) if ($i == \".debug_line\") {{ print $(i + 4); exit }} }}')\" = 80bdc24\n\
         test \"$(\"{readelf}\" --debug-dump=decodedline '{debug}' 2>/dev/null | \"{busybox}\" awk '$2 ~ /^[0-9]+$/ && $3 ~ /^0x/ {{ count++ }} END {{ print count + 0 }}')\" = 18612350\n\
         if \"{readelf}\" -S '{debug}' | \"{busybox}\" grep -E '\\.debug_(info|abbrev|aranges|ranges|rnglists|frame|loc|loclists|str)([[:space:]]|$)' >/dev/null; then exit 83; fi\n\
         test \"$(\"{busybox}\" wc -c < '{debug}')\" -le 268435456\n\
         \"{readelf}\" -l '{codex}' | \"{busybox}\" grep -F '{interpreter}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'exception.0.source=codex' '{marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'exception.0.reason=aws-lc-sys 0.39.0, ring 0.17.14, and zstd-sys 2.0.16+zstd.1.5.7 x86_64 assembly' '{marker}' >/dev/null\n\
         \"{nm}\" '{debug}' | \"{busybox}\" grep -F ' HUF_decompress4X2_usingDTable_internal_fast_asm_loop' >/dev/null\n\
         if \"{nm}\" '{debug}' | \"{busybox}\" grep -F ' blake3_' >/dev/null; then exit 84; fi\n\
         \"{busybox}\" grep -F -x 'exception.1.source=glibc-x86-64' '{marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'exception.2.source=gcc-x86-64-self' '{marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'exception.3.source=rust-toolchain' '{marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'output=codex' '{line_marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'runtime=bin/codex' '{line_marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'reader_ceiling_bytes=33554432' '{line_marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'admitted_ceiling_bytes=167772160' '{line_marker}' >/dev/null\n\
         \"{busybox}\" grep -F -x 'companion_ceiling_bytes=268435456' '{line_marker}' >/dev/null\n\
         \"{busybox}\" grep -F \"beyond td-profiler's bounded per-object reader\" '{line_marker}' >/dev/null\n\
         if \"{busybox}\" grep -a -F '/gnu/store' '{codex}' >/dev/null; then exit 82; fi\n\
         printf '%s\\n' CODEX-PACKAGE-OK\n"
    );
    let output = runner.store_ns_output(&[&busybox, "sh", "-c", &smoke], None)?;
    if !output.lines().any(|line| line == "CODEX-PACKAGE-OK") {
        return Err(format!(
            "Codex package smoke did not finish: {}",
            output.trim()
        ));
    }

    println!(
        "PASS: codex: source-built 0.148.0 CLI exposes exec/review/login/MCP/sandbox commands and carries a paired deterministic debug companion"
    );
    Ok(())
}
