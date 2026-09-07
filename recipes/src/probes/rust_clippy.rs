#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn clippy(cargo: &Path, rustc: &Path, linker: &Path, root: &Path) -> Result<Output, String> {
    Command::new(cargo)
        .args(["clippy", "--offline", "--frozen", "--message-format=json"])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(root.join("target"))
        .args(["--", "--deny", "clippy::unwrap_used"])
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("CARGO_HOME", root.join("cargo-home"))
        .env("RUSTC", rustc)
        // Start without inherited wrappers; cargo-clippy selects its own driver.
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER", linker)
        .env(
            "CARGO_ENCODED_RUSTFLAGS",
            "-Cforce-frame-pointers=yes\x1f-Cdebuginfo=line-tables-only",
        )
        .output()
        .map_err(|error| format!("run source-built Cargo/Clippy: {error}"))
}

fn require_success(output: Output, phase: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{phase}: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn main() -> Result<(), String> {
    let arguments: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [cargo, rustc, linker] = arguments.as_slice() else {
        return Err("usage: clippy-probe CARGO RUSTC LINKER".into());
    };
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    for name in ["src", "home", "cargo-home"] {
        std::fs::create_dir_all(root.join(name)).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"td-clippy-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    ).map_err(|error| error.to_string())?;
    std::fs::write(
        root.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"td-clippy-probe\"\nversion = \"0.0.0\"\n",
    )
    .map_err(|error| error.to_string())?;
    let source = root.join("src/main.rs");
    let clean = "fn main() { println!(\"source-built Clippy\"); }\n";
    std::fs::write(&source, clean).map_err(|error| error.to_string())?;
    require_success(clippy(cargo, rustc, linker, &root)?, "clean source")?;

    std::fs::write(
        &source,
        "fn main() { let _ = std::env::args().next().unwrap(); }\n",
    )
    .map_err(|error| error.to_string())?;
    let rejected = clippy(cargo, rustc, linker, &root)?;
    if rejected.status.code() != Some(101)
        || !String::from_utf8_lossy(&rejected.stdout).contains("\"code\":\"clippy::unwrap_used\"")
    {
        return Err(format!(
            "Clippy failed to reject the intended lint: {}\n{}\n{}",
            rejected.status,
            String::from_utf8_lossy(&rejected.stdout),
            String::from_utf8_lossy(&rejected.stderr)
        ));
    }
    std::fs::write(
        &source,
        "fn main() { println!(\"source-built Clippy: repaired\"); }\n",
    )
    .map_err(|error| error.to_string())?;
    require_success(clippy(cargo, rustc, linker, &root)?, "repaired source")?;
    println!("CLIPPY-PROBE-OK: clean, denied lint, repaired");
    Ok(())
}
