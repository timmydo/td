//! Build the declared native test platform, then run only its opted-in cases.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

type Result<T> = std::result::Result<T, String>;
const LINE_LIMIT: u64 = 64 * 1024;
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

struct Scratch(PathBuf);

impl Scratch {
    fn create(root: &Path) -> Result<Self> {
        let parent = root.join("target");
        std::fs::create_dir_all(&parent).map_err(|e| format!("native tool parent: {e}"))?;
        for attempt in 0..100 {
            let path = parent.join(format!(
                "native-compositor-{}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("native tool scratch: {e}")),
            }
        }
        Err("native tool scratch names exhausted".into())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only this invocation's successfully created directory is removed.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct CargoChild(Child);

impl Drop for CargoChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = crate::sys::kill_child_recorded(&mut self.0, "native test cargo cleanup");
        }
        let _ = self.0.wait();
    }
}

fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for scalar in text.chars() {
        match scalar {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn test_command(
    root: &Path,
    manifest: &str,
    binary: &Path,
    trusted: bool,
    fixture: Option<(&str, &Path, bool)>,
) -> Result<Command> {
    let tool = binary
        .to_str()
        .ok_or("native compositor path must be UTF-8")?;
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root)
        .args(["test", "--frozen", "--manifest-path", manifest])
        .args([
            "--config",
            &format!("env.TD_TEST_COMPOSITOR.value={}", toml_string(tool)),
        ])
        .args(["--config", "env.TD_TEST_COMPOSITOR.force=true"])
        .env_remove("TD_TEST_COMPOSITOR")
        .env_remove("TD_TEST_TRUSTED_ROOT")
        .env_remove("TD_EDITOR_TEST_FILE_BARRIER")
        .env_remove("TD_EDITOR_TEST_QUEUE_BARRIER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if trusted {
        cmd.args(["--config", "env.TD_TEST_TRUSTED_ROOT.value=\"1\""])
            .args(["--config", "env.TD_TEST_TRUSTED_ROOT.force=true"]);
    }
    if let Some((feature, target, library)) = fixture {
        cmd.args(["--features", feature, "--target-dir"])
            .arg(target);
        if library {
            cmd.arg("--lib");
        } else {
            cmd.args(["--test", "control_process", "native_compositor::fixture::"])
                .args(["--", "--ignored", "--test-threads=2"]);
        }
    } else {
        cmd.args(["--test", "control_process", "native_compositor::"])
            .args(["--", "--ignored", "--test-threads=2"]);
    }
    Ok(cmd)
}

fn count_field(field: &str, suffix: &str) -> Option<u64> {
    let digits = field.strip_suffix(suffix)?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    digits.parse().ok()
}

fn test_summary(line: &str) -> Option<u64> {
    let body = line.strip_prefix("test result: ok. ")?.strip_suffix('\n')?;
    let mut fields = body.split("; ");
    let passed = count_field(fields.next()?, " passed")?;
    if count_field(fields.next()?, " failed")? != 0 {
        return None;
    }
    count_field(fields.next()?, " ignored")?;
    count_field(fields.next()?, " measured")?;
    count_field(fields.next()?, " filtered out")?;
    let seconds = fields
        .next()?
        .strip_prefix("finished in ")?
        .strip_suffix('s')?;
    let (whole, fraction) = seconds.split_once('.')?;
    if whole.is_empty()
        || fraction.is_empty()
        || fields.next().is_some()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(passed)
}

fn run_cases(mut cmd: Command) -> Result<()> {
    let mut child = CargoChild(cmd.spawn().map_err(|e| format!("native cargo test: {e}"))?);
    let pipe = child
        .0
        .stdout
        .take()
        .ok_or("native cargo stdout unavailable")?;
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    let mut remaining = OUTPUT_LIMIT;
    let mut passed = None;
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take(LINE_LIMIT + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("native cargo output: {e}"))?;
        if count == 0 {
            break;
        }
        if count as u64 > LINE_LIMIT {
            return Err("native cargo output line limit".into());
        }
        remaining = remaining
            .checked_sub(count)
            .ok_or("native cargo output limit")?;
        std::io::stdout()
            .write_all(&line)
            .map_err(|e| format!("native cargo log: {e}"))?;
        let text =
            std::str::from_utf8(&line).map_err(|e| format!("native cargo output UTF-8: {e}"))?;
        if text.starts_with("test result:") {
            // A final zero or malformed summary must retire any earlier one.
            passed = test_summary(text);
        }
    }
    let status = child
        .0
        .wait()
        .map_err(|e| format!("native cargo wait: {e}"))?;
    if !status.success() {
        return Err(format!("native compositor tests failed: {status}"));
    }
    if !passed.is_some_and(|count| count > 0) {
        return Err("native compositor tests reported no passing cases".into());
    }
    Ok(())
}

pub(crate) fn run(
    root: &Path,
    manifest: &str,
    trusted: bool,
    fixture_feature: Option<&str>,
) -> Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("native test root: {e}"))?;
    let scratch = Scratch::create(&root)?;
    let status = Command::new("cargo")
        .current_dir(&root)
        .args([
            "build",
            "--frozen",
            "--manifest-path",
            "td-compositor/Cargo.toml",
        ])
        .args(["--bin", "td-compositor", "--target-dir"])
        .arg(&scratch.0)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("native compositor build: {e}"))?;
    if !status.success() {
        return Err(format!("native compositor build failed: {status}"));
    }
    // A fresh directory cannot supply an old binary if an ambient cross-target
    // setting changes Cargo's layout. Cross compilation is not a native test.
    let binary = scratch.0.join("debug/td-compositor");
    if !binary.is_file() {
        return Err("native compositor build did not produce its host binary".into());
    }
    run_cases(test_command(&root, manifest, &binary, trusted, None)?)?;
    if let Some(feature) = fixture_feature {
        // Never replace or reuse the ordinary editor's build directory.
        let target = scratch.0.join("fixture");
        run_cases(test_command(
            &root,
            manifest,
            &binary,
            trusted,
            Some((feature, &target, true)),
        )?)?;
        run_cases(test_command(
            &root,
            manifest,
            &binary,
            trusted,
            Some((feature, &target, false)),
        )?)?;
        let status = Command::new("cargo")
            .current_dir(&root)
            .args([
                "clippy",
                "--frozen",
                "--manifest-path",
                manifest,
                "--features",
                feature,
                "--target-dir",
            ])
            .arg(&target)
            .args(["--all-targets", "--", "-D", "warnings"])
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("fixture Clippy: {e}"))?;
        if !status.success() {
            return Err(format!("fixture Clippy failed: {status}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn native_command_pins_the_tool_and_only_runs_ignored_native_cases() {
        for library in [true, false] {
            let fixture = test_command(
                Path::new("/repo"),
                "td-editor/Cargo.toml",
                Path::new("/repo/tool"),
                true,
                Some(("test-file-barrier", Path::new("/repo/private"), library)),
            )
            .unwrap();
            let args: Vec<_> = fixture
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert!(args.windows(4).any(|args| args
                == [
                    "--features",
                    "test-file-barrier",
                    "--target-dir",
                    "/repo/private"
                ]));
            assert_eq!(args.contains(&"--lib"), library);
            assert_eq!(args.contains(&"native_compositor::fixture::"), !library);
            assert!(args.contains(&"env.TD_TEST_TRUSTED_ROOT.force=true"));
            assert!(fixture.get_envs().any(|(key, value)|
                key == "TD_EDITOR_TEST_FILE_BARRIER" && value.is_none()));
            assert!(fixture.get_envs().any(|(key, value)|
                key == "TD_EDITOR_TEST_QUEUE_BARRIER" && value.is_none()));
        }
        let cmd = test_command(
            Path::new("/repo"),
            "td-editor/Cargo.toml",
            Path::new("/repo/tool"),
            true,
            None,
        )
        .unwrap();
        let args: Vec<_> = cmd.get_args().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "test",
                "--frozen",
                "--manifest-path",
                "td-editor/Cargo.toml",
                "--config",
                "env.TD_TEST_COMPOSITOR.value=\"/repo/tool\"",
                "--config",
                "env.TD_TEST_COMPOSITOR.force=true",
                "--config",
                "env.TD_TEST_TRUSTED_ROOT.value=\"1\"",
                "--config",
                "env.TD_TEST_TRUSTED_ROOT.force=true",
                "--test",
                "control_process",
                "native_compositor::",
                "--",
                "--ignored",
                "--test-threads=2"
            ]
        );
        let cmd = test_command(
            Path::new("/repo"),
            "td-x/Cargo.toml",
            Path::new("/repo/tool"),
            false,
            None,
        )
        .unwrap();
        assert!(cmd
            .get_envs()
            .any(|(key, value)| key == "TD_TEST_TRUSTED_ROOT" && value.is_none()));
        assert!(cmd
            .get_envs()
            .any(|(key, value)| key == "TD_TEST_COMPOSITOR" && value.is_none()));
        assert_eq!(
            toml_string("a\"b\\c\nd\té"),
            "\"a\\\"b\\\\c\\u000ad\\u0009é\""
        );
    }

    #[test]
    #[ignore = "private subprocess for native_gate_requires_real_passing_cases"]
    fn native_gate_child_fixture() {
        let mode = std::env::var("TD_NATIVE_GATE_FIXTURE").unwrap();
        let pass = "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 14 filtered out; finished in 0.01s";
        let zero = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 16 filtered out; finished in 0.00s";
        match mode.as_str() {
            "pass" | "fail" => println!("{pass}"),
            "zero" => println!("{zero}"),
            "embedded" => println!("diagnostic: {pass}"),
            "positive-zero" => {
                println!("{pass}");
                println!("{zero}");
            }
            "malformed" => {
                println!("{pass}");
                println!("test result: ok. 2 passed; garbage");
            }
            "long" => println!("{}", "x".repeat(LINE_LIMIT as usize + 1)),
            "total" => {
                for _ in 0..513 {
                    println!("{}", "x".repeat(8191));
                }
            }
            _ => std::process::exit(2),
        }
        std::process::exit(if mode == "fail" { 1 } else { 0 });
    }

    #[test]
    fn native_gate_requires_real_passing_cases() {
        for (mode, expected) in [
            ("pass", None),
            ("zero", Some("no passing cases")),
            ("positive-zero", Some("no passing cases")),
            ("embedded", Some("no passing cases")),
            ("malformed", Some("no passing cases")),
            ("fail", Some("tests failed")),
            ("long", Some("line limit")),
            ("total", Some("output limit")),
        ] {
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args([
                "--exact",
                "native_tests::tests::native_gate_child_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("TD_NATIVE_GATE_FIXTURE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
            let result = run_cases(cmd);
            if let Some(expected) = expected {
                assert!(result.unwrap_err().contains(expected));
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    fn native_summary_is_one_exact_canonical_success_record() {
        let valid = "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 14 filtered out; finished in 0.01s\n";
        assert_eq!(test_summary(valid), Some(2));
        assert_eq!(
            test_summary(&valid.replace("2 passed", "0 passed")),
            Some(0)
        );
        for bad in [
            valid.trim_end().to_string(),
            format!("prefix: {valid}"),
            format!("{valid}trailer"),
            valid.replace("2 passed", "02 passed"),
            valid.replace("2 passed", "18446744073709551616 passed"),
            valid.replace("0 failed", "1 failed"),
            valid.replace("0.01s", "NaNs"),
            valid.replace("0.01s", "1..2s"),
        ] {
            assert_eq!(test_summary(&bad), None, "{bad:?}");
        }
    }
}
