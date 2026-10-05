//! The host's own build of a checkout crate, for `./install-apps` and
//! `./install-fonts` (install_apps.rs, install_fonts.rs): cargo, rustc and
//! a C compiler resolved as the seed provisioning resolves them, and the
//! binary taken from cargo's own report of where it put it.
//!
//! A development fixture, not host mode's jail (APPLICATIONS.md §X.7):
//! nothing built here enters a build, so the host's own cargo and C
//! compiler serve, with no static or musl requirement, and a missing piece
//! is one named line.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The host's own build tools: cargo and rustc on PATH, the C compiler as
/// the seed provisioning resolves it (`TD_CC_HOME`, else `cc` or `gcc` on
/// PATH), which also links, and the archiver beside it when there is one,
/// since a provided toolchain's `ar` may be the only one.
pub(crate) struct Tools {
    cargo: PathBuf,
    rustc: PathBuf,
    cc: PathBuf,
    ar: Option<PathBuf>,
    linker_var: String,
}

pub(crate) fn tools(root: &Path) -> Result<Tools, String> {
    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let cargo = crate::stage0::find_in_path(&penv.search_path, "cargo")
        .ok_or_else(|| "no cargo on PATH: install Rust (cargo and rustc)".to_string())?;
    let rustc = crate::stage0::find_in_path(&penv.search_path, "rustc")
        .ok_or_else(|| "no rustc on PATH beside cargo".to_string())?;
    let ccpath = match crate::stage0::provision_cc(&penv) {
        Ok(p) => p,
        Err(crate::stage0::ProvisionErr::Unavailable(m))
        | Err(crate::stage0::ProvisionErr::Broken(m)) => return Err(m),
    };
    let under = |names: &[&str]| {
        ccpath.split(':').filter(|d| !d.is_empty()).find_map(|d| {
            names
                .iter()
                .map(|name| Path::new(d).join(name))
                .find(|p| crate::stage0::is_exec(p))
        })
    };
    let cc = under(&["cc", "gcc"])
        .ok_or_else(|| format!("no cc or gcc under the provisioned C toolchain ({ccpath})"))?;
    let ar = under(&["ar"]);
    let triple = crate::stage0::rustc_host_triple(&rustc)?;
    Ok(Tools {
        cargo,
        rustc,
        cc,
        ar,
        linker_var: crate::stage0::target_linker_var(&triple),
    })
}

/// `cargo build --release --locked` for one checkout crate, the C compiler
/// pinned as compiler and linker (a host without `cc` on PATH, Guix among
/// them, has `gcc`) and rustc as found; the binary where cargo reports it,
/// which a configured target or target directory may have moved.
pub(crate) fn build(root: &Path, tools: &Tools, dir: &str, bin: &str) -> Result<PathBuf, String> {
    eprintln!("td-builder: building {dir}");
    let mut command = Command::new(&tools.cargo);
    command
        .args([
            "build",
            "--release",
            "--locked",
            "--quiet",
            "--message-format=json-render-diagnostics",
        ])
        .arg("--manifest-path")
        .arg(root.join(dir).join("Cargo.toml"))
        .env("CC", &tools.cc)
        .env("HOST_CC", &tools.cc)
        .env(&tools.linker_var, &tools.cc)
        .env("RUSTC", &tools.rustc)
        .env_remove("CARGO_BUILD_TARGET");
    if let Some(ar) = &tools.ar {
        command.env("AR", ar);
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run cargo for {dir}: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo build for {dir} failed ({})", output.status));
    }
    let path = executable(&String::from_utf8_lossy(&output.stdout), bin)
        .ok_or_else(|| format!("cargo built {dir} but reported no executable named {bin}"))?;
    if !path.is_file() {
        return Err(format!(
            "cargo built {dir} but {} is not there",
            path.display()
        ));
    }
    Ok(path)
}

/// The executable cargo's `compiler-artifact` message for the binary `bin`
/// names, the last when there are several: cargo's own word on where the
/// binary is. Every line is one JSON object; the target's name and the
/// executable are read as JSON strings, the escapes it may carry decoded.
fn executable(messages: &str, bin: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter(|line| line.contains("\"reason\":\"compiler-artifact\""))
        .filter(|line| json_string_after(line, "\"name\":\"").as_deref() == Some(bin))
        .filter_map(|line| json_string_after(line, "\"executable\":\""))
        .next_back()
        .map(PathBuf::from)
}

/// The JSON string that follows the first `key` in `line`, decoded; none
/// when the key is absent, the string unterminated or an escape not one
/// JSON allows.
fn json_string_after(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)?.checked_add(key.len())?;
    let mut chars = line.get(start..)?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => out.push(match chars.next()? {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() != 4 {
                        return None;
                    }
                    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
                }
                _ => return None,
            }),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executable_is_the_named_binarys_from_cargos_report() {
        let report = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"lib\"],\"name\":\"td-ui\"},",
            "\"executable\":null}\n",
            "{\"reason\":\"build-script-executed\",\"package_id\":\"x\"}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"name\":\"td-net\"},",
            "\"executable\":\"/x/target/release/td-net\"}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"name\":\"td-news\"},",
            "\"executable\":\"/x/a \\\"q\\\"\\\\b\\u00e9/td-news\"}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
        );
        assert_eq!(
            executable(report, "td-net"),
            Some(PathBuf::from("/x/target/release/td-net"))
        );
        assert_eq!(
            executable(report, "td-news"),
            Some(PathBuf::from("/x/a \"q\"\\bé/td-news"))
        );
        assert_eq!(executable(report, "td-mail"), None);
        assert_eq!(executable("", "td-net"), None);
        // An unterminated string or an escape JSON has not is no path.
        assert_eq!(
            json_string_after("\"executable\":\"/x/y", "\"executable\":\""),
            None
        );
        assert_eq!(
            json_string_after("\"executable\":\"/x\\q\"", "\"executable\":\""),
            None
        );
        assert_eq!(
            json_string_after("\"executable\":\"/x\\u12\"", "\"executable\":\""),
            None
        );
    }
}
