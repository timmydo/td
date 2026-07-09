use crate::check_runner::{is_executable, RecipeCheckRunner, TD_STORE_DIR};

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("rust-toolchain")?;
    let build_out = runner.build_plan("rust-toolchain")?;
    let rust_tree = runner.ladder_out_from(&build_out, "rust-toolchain")?;
    println!(
        "   [ladder] x86_64 rust-toolchain via build-plan --auto: catalog dependency closure -> rust-toolchain (relinked rustc/cargo tree {})",
        rust_tree.display()
    );
    let rustc = rust_tree.join("bin/rustc");
    let cargo = rust_tree.join("bin/cargo");
    if !is_executable(&rustc) {
        return Err(format!(
            "rustc missing from rust-toolchain output ({})",
            rustc.display()
        ));
    }
    if !is_executable(&cargo) {
        return Err(format!(
            "cargo missing from rust-toolchain output ({})",
            cargo.display()
        ));
    }
    let rbase = rust_tree
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            format!(
                "malformed rust-toolchain output path {}",
                rust_tree.display()
            )
        })?;
    let rpath = format!("{TD_STORE_DIR}/{rbase}");
    let rustc_version =
        runner.store_ns_output(&[&format!("{rpath}/bin/rustc"), "--version"], None)?;
    if !rustc_version.starts_with("rustc 1.96.0") {
        return Err(format!(
            "rustc version did not match the pinned 1.96.0 release: {}",
            rustc_version.trim()
        ));
    }
    runner.store_ns_output(&[&format!("{rpath}/bin/cargo"), "--version"], None)?;
    runner.store_ns_output(
        &[
            &format!("{rpath}/bin/rustc"),
            "--crate-name",
            "td_rust_smoke",
            "--crate-type=lib",
            "--edition=2021",
            "-",
            "-o",
            "/tmp/librust_smoke.rlib",
        ],
        Some("pub fn td_rust_smoke() -> u32 { 42 }\n"),
    )?;
    println!(
        "PASS: rust-toolchain: Rust 1.96.0 relinked onto /td/store runs rustc/cargo and compiles a simple library"
    );
    Ok(())
}
