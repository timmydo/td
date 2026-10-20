//! Offline host/sandbox qualification of the named private crypto closure.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type Result<T> = std::result::Result<T, String>;

pub(crate) fn validate(root: &Path) -> Result<()> {
    crate::crypto_policy::cargo_config(root)?;
    validate_sources(root)
}

pub(crate) fn validate_sources(root: &Path) -> Result<()> {
    for name in crate::crypto_policy::LOCAL_SOURCES {
        crate::crypto_policy::no_build_script(root, name)?;
        for file in ["Cargo.toml", "Cargo.lock"] {
            let text = std::fs::read_to_string(root.join(name).join(file))
                .map_err(|e| format!("read {name}/{file}: {e}"))?;
            if file == "Cargo.toml" {
                crate::crypto_policy::manifest_pin(name, &text)?;
            } else {
                crate::crypto_policy::lock_pin(name, &text)?;
            }
        }
    }
    Ok(())
}

pub(crate) const CONTROLS: &[(&str, &str)] = &[
    ("AWS_LC_SYS_USE_SYSTEM", "0"),
    ("AWS_LC_SYS_CMAKE_BUILDER", "0"),
    ("AWS_LC_SYS_PREBUILT_NASM", "0"),
    ("AWS_LC_SYS_STATIC", "1"),
    ("AWS_LC_SYS_EXTERNAL_BINDGEN", "0"),
];

fn reserved_control(key: &str) -> bool {
    key.starts_with("AWS_LC_SYS_")
}

fn check_controls(vars: impl Iterator<Item = (String, String)>) -> Result<()> {
    for (key, value) in vars {
        if reserved_control(&key) && !CONTROLS.iter().any(|(k, v)| *k == key && *v == value) {
            return Err(format!("unsupported native mail build override: {key}"));
        }
    }
    Ok(())
}

struct Scratch(PathBuf);
impl Scratch {
    fn create(root: &Path) -> Result<Self> {
        let parent = root.join(".td-build-cache");
        std::fs::create_dir_all(&parent).map_err(|e| format!("crypto build parent: {e}"))?;
        for attempt in 0..100 {
            let path = parent.join(format!("crypto-home-{}-{attempt}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("crypto Cargo home: {e}")),
            }
        }
        Err("crypto Cargo home names exhausted".into())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = crate::sys::kill_child_recorded(&mut self.0, "crypto Cargo cleanup");
        }
        let _ = self.0.wait();
    }
}

pub(crate) fn run(root: &Path, action: &str, manifest: &str) -> Result<()> {
    run_inner(root, action, manifest).map(|_| ())
}

pub(crate) struct Release {
    pub(crate) executable: PathBuf,
    pub(crate) vendor: PathBuf,
    pub(crate) receipt: String,
}

pub(crate) fn release(root: &Path) -> Result<Release> {
    run_inner(root, "build", "td-mta/Cargo.toml")?
        .ok_or("release build returned no executable".into())
}

fn run_inner(root: &Path, action: &str, manifest: &str) -> Result<Option<Release>> {
    if !matches!(action, "test" | "clippy" | "build") {
        return Err("crypto-cargo accepts only test or clippy".into());
    }
    let name = match manifest {
        "td-crypto/Cargo.toml" => "td-crypto",
        "td-mta/Cargo.toml" => "td-mta",
        _ => return Err("manifest has no crypto admission".into()),
    };
    validate(root)?;
    let variables = std::env::vars_os()
        .map(|(key, value)| {
            let key = key
                .into_string()
                .map_err(|_| "build environment key is not UTF-8")?;
            if reserved_control(&key) {
                Ok((
                    key,
                    value
                        .into_string()
                        .map_err(|_| "crypto build control is not UTF-8")?,
                ))
            } else {
                Ok((key, String::new()))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    check_controls(variables.into_iter())?;
    let home = Scratch::create(root)?;
    let vendor = crate::host_bin::prepare_mail_vendor(root)?;
    let vendor = vendor
        .canonicalize()
        .map_err(|e| format!("crypto vendor path: {e}"))?;
    let vendor = vendor.to_str().ok_or("crypto vendor path must be UTF-8")?;
    let quoted = td_engine::json::Json::Str(vendor.into()).to_json_string();
    let penv = crate::stage0::ProvisionEnv::from_env(root);
    let rustpath = crate::stage0::provision_rust(&penv).map_err(|e| e.to_string())?;
    let ccpath = crate::stage0::provision_cc(&penv).map_err(|e| e.to_string())?;
    let find = |path: &str, binary: &str| {
        crate::stage0::find_in_path(path, binary)
            .ok_or_else(|| format!("crypto build requires {binary} in provisioned toolchain"))
    };
    let cargo = find(&rustpath, "cargo")?;
    let rustc = find(&rustpath, "rustc")?;
    let rustdoc = find(&rustpath, "rustdoc")?;
    let cc = crate::stage0::host_cc(&ccpath)
        .ok_or("crypto build requires cc or gcc in provisioned toolchain")?;
    let ar = find(&ccpath, "ar")?;
    let host = crate::stage0::rustc_host_triple(&rustc)?;
    if !matches!(
        host.as_str(),
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl"
    ) {
        return Err(format!(
            "crypto host qualification is not yet defined for {host}"
        ));
    }
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .unwrap_or_else(|| root.join(".td-build-cache/crypto-target").into_os_string());
    let native_flags = format!(
        "-march=x86-64 -mtune=generic -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer -g1 -ffile-prefix-map={}=/td-build -ffile-prefix-map={vendor}=/td-cargo/vendor -ffile-prefix-map={}=/td-build-root",
        root.display(), Path::new(&target_dir).display()
    );
    let command = |verb: &str, manifest: &str| {
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(root)
            .args([
                verb,
                "--frozen",
                "--manifest-path",
                manifest,
                "--target",
                &host,
            ])
            .args([
                "--config",
                "source.crates-io.replace-with=\"td-crypto-vendor\"",
            ])
            .args([
                "--config",
                &format!("source.td-crypto-vendor.directory={quoted}"),
            ])
            .env("CARGO_HOME", &home.0)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env(
                "PATH",
                format!(
                    "{rustpath}:{ccpath}:{}",
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("RUSTC", &rustc)
            .env("RUSTDOC", &rustdoc)
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("CARGO_ENCODED_RUSTFLAGS", "")
            .env_remove("TD_MTA_TEST_MAIL")
            .env_remove("TD_MTA_TEST_FETCH")
            .env_remove("TD_MTA_TEST_TLS_PEER")
            .env_remove("TD_MTA_PORTABLE_CLI")
            .env(crate::stage0::target_linker_var(&host), &cc)
            .stdin(Stdio::null());
        // cc-rs recognizes target-specific spellings before plain CC/AR.
        for prefix in ["", "HOST_", "TARGET_"] {
            cmd.env(format!("{prefix}CC"), &cc)
                .env(format!("{prefix}AR"), &ar)
                .env(format!("{prefix}CFLAGS"), &native_flags);
        }
        for target in [host.clone(), host.replace('-', "_")] {
            cmd.env(format!("CC_{target}"), &cc)
                .env(format!("AR_{target}"), &ar)
                .env(format!("CFLAGS_{target}"), &native_flags);
        }
        for (key, value) in CONTROLS {
            cmd.env(key, value);
            // A repository [env] entry with force=true must not override the
            // selected native build path after Cargo reads our environment.
            cmd.args(["--config", &format!("env.{key}.value=\"{value}\"")]);
            cmd.args(["--config", &format!("env.{key}.force=true")]);
        }
        crate::host_bin::arm_check_child(&mut cmd);
        cmd
    };
    let output = command("tree", manifest)
        .args([
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--no-dedupe",
            "--format",
            "{p}|{f}",
        ])
        .output()
        .map_err(|e| format!("crypto active graph: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "crypto active graph failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let graph =
        std::str::from_utf8(&output.stdout).map_err(|e| format!("crypto graph UTF-8: {e}"))?;
    crate::crypto_policy::active_graph(root, name, graph)?;
    let mut cmd = command(action, manifest);
    if action == "build" {
        std::fs::create_dir_all(&target_dir).map_err(|e| format!("release target: {e}"))?;
        let target = Path::new(&target_dir)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        cmd.args(["--release", "--bin", "td-mta"])
            .env("CARGO_PROFILE_RELEASE_DEBUG", "1")
            .env("CARGO_PROFILE_RELEASE_STRIP", "none");
        let mut receipt = String::from("td-mta native host release; debug retained in executable\nNot portable or source-bootstrapped. Retain host compiler/runtime packages.\n");
        receipt.push_str(&format!("command {cmd:?}\n"));
        let executable = fixture_executable(
            cmd,
            "td-mta",
            crate::crypto_isolated::ArtifactKind::Installed,
            &target,
        )?;
        return Ok(Some(Release {
            executable,
            vendor: PathBuf::from(vendor),
            receipt,
        }));
    }
    let mut live_fixture = None;
    if action == "test" && name == "td-mta" {
        std::fs::create_dir_all(&target_dir).map_err(|e| format!("crypto fixture target: {e}"))?;
        let target = Path::new(&target_dir)
            .canonicalize()
            .map_err(|e| format!("crypto fixture target: {e}"))?;
        let mut peer = command("test", "td-crypto/Cargo.toml");
        peer.args(["--lib", "--no-run"]);
        cmd.env(
            "TD_MTA_TEST_TLS_PEER",
            fixture_executable(
                peer,
                "td_crypto",
                crate::crypto_isolated::ArtifactKind::LibraryTest,
                &target,
            )?,
        );
        let mail = fixture_executable(
            command("build", "td-mail/Cargo.toml"),
            "td-mail",
            crate::crypto_isolated::ArtifactKind::Installed,
            &target,
        )?;
        let net_vendor = crate::host_bin::prepare_native_vendor(root)?;
        let net_vendor = net_vendor
            .to_str()
            .ok_or("fetch fixture vendor path must be UTF-8")?;
        let quoted = td_engine::json::Json::Str(net_vendor.into()).to_json_string();
        let mut fetch = command("test", "net/Cargo.toml");
        fetch.args([
            "--bin",
            "td-net",
            "--no-run",
            "--config",
            &format!("source.td-crypto-vendor.directory={quoted}"),
        ]);
        let fetch = fixture_executable(
            fetch,
            "td-net",
            crate::crypto_isolated::ArtifactKind::BinaryTest,
            &target,
        )?;
        live_fixture = Some((mail, fetch));
    }
    if action == "clippy" {
        cmd.args(["--all-targets", "--", "-D", "warnings"]);
    }
    let mut child = Child(cmd.spawn().map_err(|e| format!("crypto {action}: {e}"))?);
    let status = child
        .0
        .wait()
        .map_err(|e| format!("crypto {action} wait: {e}"))?;
    if !status.success() {
        return Err(format!("crypto {action} failed: {status}"));
    }
    if let Some((mail, fetch)) = live_fixture {
        let postgres = std::env::var_os("TD_MTA_TEST_POSTGRES_BIN")
            .map(PathBuf::from)
            .ok_or("native mail tests require TD_MTA_TEST_POSTGRES_BIN pointing to PostgreSQL 17 binaries")?;
        if !postgres.is_absolute()
            || ["initdb", "postgres", "pg_ctl", "psql"]
                .iter()
                .any(|name| !postgres.join(name).is_file())
        {
            return Err(
                "TD_MTA_TEST_POSTGRES_BIN must be an absolute PostgreSQL binary directory".into(),
            );
        }
        for case in [
            "real_postgres_td_mail_reads_and_downloads",
            "real_postgres_td_mail_sends_plain_text",
            "real_postgres_executable_receives_and_serves_database_profiles",
        ] {
            let mut fixture = command("test", manifest);
            fixture
                .env("TD_MTA_TEST_MAIL", &mail)
                .env("TD_MTA_TEST_FETCH", &fetch)
                .env("TD_MTA_TEST_POSTGRES_BIN", &postgres)
                .env("TD_TEST_TRUSTED_ROOT", "private")
                .args([
                    "--test",
                    "postgres_probe",
                    "--",
                    "--exact",
                    case,
                    "--ignored",
                    "--nocapture",
                ]);
            fixture.stdout(Stdio::piped());
            crate::native_tests::run_cases(fixture)?;
        }
    }
    Ok(None)
}

fn fixture_executable(
    mut command: Command,
    name: &str,
    kind: crate::crypto_isolated::ArtifactKind,
    target: &Path,
) -> Result<PathBuf> {
    use std::io::Read;
    command
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped());
    let mut child = Child(
        command
            .spawn()
            .map_err(|e| format!("{name} fixture build: {e}"))?,
    );
    let mut output = Vec::new();
    child
        .0
        .stdout
        .take()
        .ok_or("fixture build has no stdout")?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut output)
        .map_err(|e| format!("read {name} fixture build: {e}"))?;
    if output.len() > 8 * 1024 * 1024 {
        return Err("fixture build output exceeds ceiling".into());
    }
    if !child
        .0
        .wait()
        .map_err(|e| format!("{name} fixture wait: {e}"))?
        .success()
    {
        return Err(format!("{name} fixture build failed"));
    }
    let messages = std::str::from_utf8(&output).map_err(|e| format!("{name} fixture JSON: {e}"))?;
    let executable = crate::crypto_isolated::artifact_path(messages, name, kind)?
        .canonicalize()
        .map_err(|e| format!("{name} fixture executable: {e}"))?;
    if !executable.starts_with(target) || !executable.is_file() {
        return Err(format!("{name} fixture escaped target directory"));
    }
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_controls_refuse_fallbacks_and_target_overrides() {
        assert!(check_controls(CONTROLS.iter().map(|&(k, v)| (k.into(), v.into()))).is_ok());
        for (key, value) in [
            ("AWS_LC_SYS_USE_SYSTEM", "1"),
            ("AWS_LC_SYS_CMAKE_BUILDER", "1"),
            ("AWS_LC_SYS_PREBUILT_NASM", "1"),
            ("AWS_LC_SYS_STATIC", "0"),
            ("AWS_LC_SYS_CMAKE_BUILDER_x86_64_unknown_linux_gnu", "1"),
            ("AWS_LC_SYS_EXTERNAL_BINDGEN", "1"),
        ] {
            assert!(check_controls(std::iter::once((key.into(), value.into()))).is_err());
        }
    }
}
