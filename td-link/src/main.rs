//! td-link: rustc's linker. It reads the gcc-style link line rustc writes,
//! adds td glibc's and gcc-runtime's start files, search paths and loader,
//! and runs the toolchain's rust-lld in its GNU flavor. It never compiles.
#![forbid(unsafe_code)]

mod plan;
mod response;

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, ExitStatus};

use plan::Toolchain;

/// `E2BIG`: the argument list did not fit, so retry through a response file.
const ARGUMENT_LIST_TOO_LONG: i32 = 7;

fn compiled() -> Result<Toolchain<'static>, String> {
    Ok(Toolchain {
        lld: option_env!("TD_LINK_LLD")
            .ok_or("td-link needs its target recipe's rust-lld configuration")?,
        glibc: option_env!("TD_LINK_GLIBC")
            .ok_or("td-link needs its target recipe's glibc configuration")?,
        runtime: option_env!("TD_LINK_RUNTIME")
            .ok_or("td-link needs its target recipe's gcc-runtime configuration")?,
    })
}

/// LLD's own response file, beside the output it names.
fn response_path(output: &OsString) -> PathBuf {
    let mut path = output.clone().into_vec();
    path.extend_from_slice(b".td-link-args");
    PathBuf::from(OsString::from_vec(path))
}

fn run_lld(
    lld: &str,
    args: &[OsString],
    via_file: bool,
    output: &OsString,
) -> Result<ExitStatus, String> {
    let mut command = Command::new(lld);
    command.args(["-flavor", "gnu"]);
    if !via_file {
        match command.args(args).status() {
            Err(e) if e.raw_os_error() == Some(ARGUMENT_LIST_TOO_LONG) => {
                return run_lld(lld, args, true, output);
            }
            status => return status.map_err(|e| format!("td-link: execute {lld}: {e}")),
        }
    }
    let file = response_path(output);
    std::fs::write(&file, response::encode(args))
        .map_err(|e| format!("td-link: write {}: {e}", file.display()))?;
    let mut argument = OsString::from("@");
    argument.push(&file);
    let status = command
        .arg(argument)
        .status()
        .map_err(|e| format!("td-link: execute {lld}: {e}"));
    let removed =
        std::fs::remove_file(&file).map_err(|e| format!("td-link: remove {}: {e}", file.display()));
    let status = status?;
    removed?;
    Ok(status)
}

/// Cargo and the native bootstrap pin RUSTC before isolating HOME. Keep
/// the linker with that compiler instead of preparing a second cache.
fn runtime_lld(rustc: Option<OsString>) -> Result<String, String> {
    let mut command = if let Some(rustc) = rustc {
        let mut command = Command::new(rustc);
        command.args(["--print", "sysroot"]);
        command
    } else {
        let mut command = Command::new("/bin/td-rust");
        command.arg("path");
        command
    };
    let output = command
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("td-link: resolve Rust: {e}"))?;
    if !output.status.success() {
        return Err(format!("td-link: resolve Rust exited {}", output.status));
    }
    let root = std::str::from_utf8(&output.stdout)
        .map_err(|_| "td-link: invalid Rust path")?
        .trim_end();
    if !std::path::Path::new(root).is_absolute() || root.contains(['\n', '\r', '\0']) {
        return Err("td-link: invalid Rust path".into());
    }
    Ok(format!(
        "{root}/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld"
    ))
}

fn run() -> Result<ExitCode, String> {
    let compiled = compiled()?;
    let downloaded;
    let toolchain = if option_env!("TD_LINK_ON_DEMAND") == Some("1") {
        downloaded = runtime_lld(std::env::var_os("RUSTC"))?;
        Toolchain {
            lld: &downloaded,
            glibc: compiled.glibc,
            runtime: compiled.runtime,
        }
    } else {
        compiled
    };
    let (args, response) = response::expand(std::env::args_os().skip(1).collect())?;
    let plan = plan::plan(args, &toolchain)?;
    plan.check_inputs()?;
    let status = run_lld(
        toolchain.lld,
        &plan.ld_args(&toolchain),
        response,
        &plan.path,
    )?;
    Ok(match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        (None, Some(signal)) => ExitCode::from(u8::try_from(128 + signal).unwrap_or(1)),
        (None, None) => ExitCode::FAILURE,
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_fs::TempDir;

    #[test]
    fn a_host_build_without_the_recipe_configuration_refuses_to_link() {
        if option_env!("TD_LINK_LLD").is_none() {
            assert!(compiled().unwrap_err().contains("target recipe"));
        }
    }

    #[test]
    fn runtime_linker_uses_selected_compiler_without_cache_lookup() {
        let dir = TempDir::new("td-link-selected-rust").unwrap();
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(&compiler)
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let root = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            runtime_lld(Some(compiler)).unwrap(),
            format!(
                "{}/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
                root.trim_end()
            )
        );
        assert!(runtime_lld(Some(dir.join("missing-compiler").into_os_string())).is_err());
    }

    /// The response file goes beside the output, holds the arguments LLD
    /// reads back unchanged, and is gone once the link returns.
    #[test]
    fn a_response_file_link_hands_lld_its_arguments_and_cleans_up() {
        let dir = TempDir::new("td-link-rsp").unwrap();
        let output = OsString::from(dir.join("out with space"));
        let seen = dir.join("seen");
        let fake = dir.join("lld");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = '-flavor gnu' ] || exit 3\ncat \"${{3#@}}\" > '{}'\n",
                seen.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let args = vec![
            OsString::from("-o"),
            output.clone(),
            OsString::from("a b.o"),
        ];
        let status = run_lld(fake.to_str().unwrap(), &args, true, &output).unwrap();
        assert!(status.success());
        assert_eq!(
            response::parse(&std::fs::read(&seen).unwrap()),
            args,
            "LLD reads back exactly the planned arguments"
        );
        assert!(!response_path(&output).exists());
    }
}
