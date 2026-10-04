//! `td-builder host-run NAME [ARG...]` — the checkout's td-news, td-mail or
//! td-agent on this host, unjailed, with the fetch service it needs served for
//! it. The repository-root `./news`, `./mail` and `./agent` entry scripts exec
//! this.
//!
//! A development fixture, not host mode's jail (APPLICATIONS.md §X.7): the
//! application runs as the caller under the session's Wayland display, and
//! nothing built here enters a build, so the host's own cargo and C compiler
//! serve, with no static or musl requirement, and a missing piece is one
//! named line.
//!
//! The one thing the applications cannot do themselves is fetch: they hold no
//! TLS, resolver or network and ask `td-fetchd` at
//! `$XDG_RUNTIME_DIR/td-fetch/socket`. This verb builds the checkout's td-net
//! multicall and the application and becomes `td-net launch APP`, which
//! serves the socket in a runtime directory of the launch's own and runs the
//! application there (net/src/launch.rs).

use std::convert::Infallible;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// The application crate a launcher name stands for; the crate is its
/// binary's name too.
fn crate_of(name: &str) -> Option<&'static str> {
    match name {
        "news" => Some("td-news"),
        "mail" => Some("td-mail"),
        "agent" => Some("td-agent"),
        _ => None,
    }
}

/// The programs an application runs beside it, each a checkout crate built
/// as its binary and named to the application in an environment variable:
/// td-agent's workspace jail and the td-txt its tools run
/// (td-agent/DESIGN.md §8, td-agent/src/jail.rs).
fn companions(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "agent" => &[("td-jail", "TD_AGENT_JAIL"), ("td-txt", "TD_AGENT_TXT")],
        _ => &[],
    }
}

/// The checkout, which is the working directory: the entry scripts change
/// to it, and a verb run from anywhere else is told so before anything is
/// built.
fn checkout(root: &Path, crate_dir: &str) -> Result<(), String> {
    for dir in ["net", crate_dir] {
        if !root.join(dir).join("Cargo.toml").is_file() {
            return Err(format!(
                "{} is not the td checkout ({dir}/Cargo.toml is not under it): run ./news, \
                 ./mail or ./agent from the repository root",
                root.display()
            ));
        }
    }
    Ok(())
}

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
    eprintln!("host-run: building {dir}");
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

/// What `td-net launch` will refuse, said before the builds rather than
/// after: a runtime directory that is unset or not absolute.
fn runtime_dir(runtime: Option<&std::ffi::OsStr>) -> Result<(), String> {
    match runtime.filter(|dir| !dir.is_empty()).map(Path::new) {
        Some(dir) if dir.is_absolute() => Ok(()),
        Some(dir) => Err(format!(
            "XDG_RUNTIME_DIR={} is not an absolute path",
            dir.display()
        )),
        None => Err(
            "XDG_RUNTIME_DIR is not set: the fetch socket and the Wayland display \
                     live under it (a session manager such as elogind sets it)"
                .to_string(),
        ),
    }
}

/// Build td-net and the application and become `td-net launch APP`;
/// returns only why that could not happen.
fn launch(root: &Path, name: &str, args: &[String]) -> Result<Infallible, String> {
    let crate_dir = crate_of(name).ok_or_else(|| format!("no application named {name}"))?;
    checkout(root, crate_dir)?;
    runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref())?;
    let tools = tools(root)?;
    let td_net = build(root, &tools, "net", "td-net")?;
    let app = build(root, &tools, crate_dir, crate_dir)?;
    let mut command = Command::new(&td_net);
    for (crate_dir, var) in companions(name) {
        command.env(var, build(root, &tools, crate_dir, crate_dir)?);
    }
    eprintln!("host-run: running {} under td-net launch", app.display());
    // td-net's launch serves the fetch socket and becomes the application,
    // as this process becomes it: the application's exit is this verb's.
    let e = command.arg("launch").arg(&app).args(args).exec();
    Err(format!("cannot run {} launch: {e}", td_net.display()))
}

/// `args` are the verb's own: the name, then the application's arguments.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(name) = args.first().map(String::as_str) else {
        eprintln!("usage: td-builder host-run news|mail|agent [ARG...]");
        return ExitCode::from(2);
    };
    if crate_of(name).is_none() {
        eprintln!("usage: td-builder host-run news|mail|agent [ARG...]");
        return ExitCode::from(2);
    }
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("td-builder: host-run: getcwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    match launch(&root, name, args.get(1..).unwrap_or(&[])) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("td-builder: host-run: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_directory_is_checked_before_anything_is_built() {
        use std::ffi::OsStr;
        assert!(runtime_dir(Some(OsStr::new("/run/user/1"))).is_ok());
        assert!(runtime_dir(Some(OsStr::new("run/user/1")))
            .unwrap_err()
            .contains("not an absolute path"));
        assert!(runtime_dir(Some(OsStr::new("")))
            .unwrap_err()
            .starts_with("XDG_RUNTIME_DIR is not set"));
        assert!(runtime_dir(None)
            .unwrap_err()
            .starts_with("XDG_RUNTIME_DIR is not set"));
    }

    #[test]
    fn the_names_are_the_three_applications() {
        assert_eq!(crate_of("news"), Some("td-news"));
        assert_eq!(crate_of("mail"), Some("td-mail"));
        assert_eq!(crate_of("agent"), Some("td-agent"));
        assert_eq!(crate_of("td-news"), None);
        assert_eq!(crate_of("td-agent"), None);
        assert_eq!(crate_of(""), None);
    }

    #[test]
    fn the_agent_is_given_its_jail_and_td_txt() {
        assert_eq!(
            companions("agent"),
            [("td-jail", "TD_AGENT_JAIL"), ("td-txt", "TD_AGENT_TXT")]
        );
        assert!(companions("news").is_empty());
        assert!(companions("mail").is_empty());
    }

    #[test]
    fn the_checkout_is_named_when_the_directory_is_not_it() {
        let e = checkout(Path::new("/nonexistent"), "td-news").unwrap_err();
        assert!(e.contains("net/Cargo.toml"), "{e}");
        assert!(e.ends_with("from the repository root"), "{e}");
    }

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
