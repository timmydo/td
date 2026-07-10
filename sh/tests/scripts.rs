//! Behavioral coverage for td-sh as the bootstrap build shell. Every case is a
//! construct the mesboot ladder's build scripts actually exercise (mes-0.27.1
//! configure.sh/bootstrap.sh, autotools configure, make's `$(SHELL) -c`
//! recipe lines) — the compatibility floor td-sh must hold to replace the
//! declared bash input. Validated end-to-end besides this: mes-0.27.1
//! configure.sh AND bootstrap.sh (the mescc self-build) complete under td-sh
//! with the stage0 seed tools on PATH (see the introducing PR).

use std::process::{Command, Output};

fn td_sh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_td-sh"))
}

fn run_c(script: &str) -> Result<Output, String> {
    td_sh()
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|e| format!("spawn td-sh: {e}"))
}

fn stdout_of(script: &str) -> Result<String, String> {
    let out = run_c(script)?;
    if !out.status.success() {
        return Err(format!(
            "td-sh -c {script:?} failed: {:?}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `make` runs every recipe line as `$(SHELL) -c '…'`.
#[test]
fn dash_c_runs_a_command() -> Result<(), String> {
    let out = stdout_of("echo hello")?;
    if out.trim() != "hello" {
        return Err(format!("expected 'hello', got {out:?}"));
    }
    Ok(())
}

/// mes configure.sh runs under `set -e`: a failing command must abort the
/// script before later commands run.
#[test]
fn set_e_aborts_on_failure() -> Result<(), String> {
    let out = run_c("set -e; false; echo NOT_REACHED")?;
    if out.status.success() {
        return Err("set -e; false did not fail the script".into());
    }
    if String::from_utf8_lossy(&out.stdout).contains("NOT_REACHED") {
        return Err("set -e did not stop execution at the failing command".into());
    }
    Ok(())
}

/// configure scripts probe tools with `command -v` (mes configure.sh does this
/// for blood-elf/M1/hex2) and branch on the result under set -e via `|| true`.
#[test]
fn command_v_probes() -> Result<(), String> {
    let out = stdout_of(
        "set -e; \
         if command -v definitely-not-a-real-tool >/dev/null 2>&1; then echo found; else echo missing; fi",
    )?;
    if out.trim() != "missing" {
        return Err(format!("command -v probe: expected 'missing', got {out:?}"));
    }
    Ok(())
}

/// Heredocs with parameter/arithmetic/command-substitution expansion — the
/// bread and butter of generated configure output files.
#[test]
fn heredoc_with_expansions() -> Result<(), String> {
    let out = stdout_of("v=mes; cat <<EOF\nname=$v sum=$((40 + 2)) sub=$(echo ok)\nEOF")?;
    if out.trim() != "name=mes sum=42 sub=ok" {
        return Err(format!("heredoc expansion: got {out:?}"));
    }
    Ok(())
}

/// case/for/shell-function composition — mes configure.sh parses its argv with
/// a `while … case` loop and defines helper functions.
#[test]
fn case_for_and_functions() -> Result<(), String> {
    let out = stdout_of(
        "greet() { echo \"hi $1\"; }; \
         for x in a b; do case $x in a) greet A;; *) greet other;; esac; done",
    )?;
    if out.trim() != "hi A\nhi other" {
        return Err(format!("case/for/function: got {out:?}"));
    }
    Ok(())
}

/// Prefix/suffix parameter expansion and defaults (`${x#…}`, `${x%…}`,
/// `${x-default}`) — used throughout mes configure.sh option parsing.
#[test]
fn parameter_expansion() -> Result<(), String> {
    let out = stdout_of(
        "opt=--prefix=/td/store; echo \"${opt#--prefix=}\"; \
         f=lib/mes.c; echo \"${f%.c}\"; echo \"${unset_var-fallback}\"",
    )?;
    if out.trim() != "/td/store\nlib/mes\nfallback" {
        return Err(format!("parameter expansion: got {out:?}"));
    }
    Ok(())
}

/// `eval` composing a command with a redirect — mes build-aux/trace.sh runs
/// every compile step as `eval $cmd $LOG` with LOG=' >.log 2>&1'.
#[test]
fn eval_with_redirect_string() -> Result<(), String> {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let log = format!("{dir}/td-sh-eval-test.log");
    let _ = std::fs::remove_file(&log);
    let out = run_c(&format!(
        "LOG=' >{log} 2>&1'; cmd='echo traced'; eval $cmd $LOG"
    ))?;
    if !out.status.success() {
        return Err("eval with redirect string failed".into());
    }
    let body = std::fs::read_to_string(&log).map_err(|e| format!("read {log}: {e}"))?;
    if body.trim() != "traced" {
        return Err(format!("eval redirect wrote {body:?}"));
    }
    Ok(())
}

/// A script FILE with positional args — how the rungs invoke every build
/// script (`{in:bash}/bin/bash configure.sh --prefix=…`).
#[test]
fn script_file_with_args() -> Result<(), String> {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let path = format!("{dir}/td-sh-script-test.sh");
    std::fs::write(&path, "echo \"argc=$# first=$1\"\nexit 0\n")
        .map_err(|e| format!("write {path}: {e}"))?;
    let out = td_sh()
        .arg(&path)
        .args(["--prefix=/td/store", "--host=i686-linux-gnu"])
        .output()
        .map_err(|e| format!("spawn td-sh: {e}"))?;
    if !out.status.success() {
        return Err(format!("script file run failed: {:?}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "argc=2 first=--prefix=/td/store" {
        return Err(format!("script args: got {stdout:?}"));
    }
    Ok(())
}

/// Commands piped on stdin (`… | sh` constructs inside build scripts).
#[test]
fn reads_commands_from_stdin() -> Result<(), String> {
    use std::io::Write as _;
    let mut child = td_sh()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn td-sh: {e}"))?;
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin
            .write_all(b"echo from-stdin\n")
            .map_err(|e| format!("write stdin: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait td-sh: {e}"))?;
    if !out.status.success() {
        return Err(format!("stdin script failed: {:?}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "from-stdin" {
        return Err(format!("stdin script: got {stdout:?}"));
    }
    Ok(())
}

/// Pipelines and exit-status propagation (`gcc … 2>&1 | tee` patterns; the
/// rungs also grep compile logs in pipelines).
#[test]
fn pipeline_and_exit_status() -> Result<(), String> {
    let out = stdout_of("printf 'b\\na\\n' | sort | head -n1")?;
    if out.trim() != "a" {
        return Err(format!("pipeline: got {out:?}"));
    }
    let code = run_c("exit 7")?.status.code();
    if code != Some(7) {
        return Err(format!("exit 7 reported {code:?}"));
    }
    Ok(())
}

/// `--sh` runs in sh/POSIX compatibility mode.
#[test]
fn sh_mode_runs_posix_scripts() -> Result<(), String> {
    let out = td_sh()
        .args(["--sh", "-c", "x=posix; test \"$x\" = posix && echo ok"])
        .output()
        .map_err(|e| format!("spawn td-sh: {e}"))?;
    if !out.status.success() {
        return Err(format!("--sh mode script failed: {:?}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "ok" {
        return Err(format!("--sh mode: got {stdout:?}"));
    }
    Ok(())
}

/// A `sh` SYMLINK to td-sh (the eventual `bin/sh` alias) must get sh mode,
/// not silently keep full bash semantics: brush itself does no argv[0]
/// detection (cross-model review finding on the introducing PR), so the
/// wrapper re-execs with --sh injected. `shopt` is a bash-only builtin —
/// resolvable in bash mode, unknown in sh mode.
#[test]
fn sh_alias_switches_to_sh_mode() -> Result<(), String> {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let alias = format!("{dir}/sh");
    let _ = std::fs::remove_file(&alias);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_td-sh"), &alias)
        .map_err(|e| format!("symlink {alias}: {e}"))?;
    let probe = "if type shopt >/dev/null 2>&1; then echo bash-mode; else echo sh-mode; fi";
    let out = Command::new(&alias)
        .args(["-c", probe])
        .output()
        .map_err(|e| format!("spawn sh alias: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "sh-mode" {
        return Err(format!("sh alias: expected sh-mode, got {stdout:?}"));
    }
    // The plain td-sh name stays in bash mode — the rungs replace bash.
    let out = run_c(probe)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "bash-mode" {
        return Err(format!("td-sh name: expected bash-mode, got {stdout:?}"));
    }
    Ok(())
}

/// The alias re-exec must preserve script arguments across the boundary.
#[test]
fn sh_alias_preserves_args() -> Result<(), String> {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let alias = format!("{dir}/sh-args-alias/sh");
    let _ = std::fs::remove_dir_all(format!("{dir}/sh-args-alias"));
    std::fs::create_dir_all(format!("{dir}/sh-args-alias"))
        .map_err(|e| format!("mkdir: {e}"))?;
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_td-sh"), &alias)
        .map_err(|e| format!("symlink {alias}: {e}"))?;
    let script = format!("{dir}/sh-args-alias/probe.sh");
    std::fs::write(&script, "echo \"n=$# one=$1 two=$2\"\n")
        .map_err(|e| format!("write {script}: {e}"))?;
    let out = Command::new(&alias)
        .args([script.as_str(), "alpha", "beta"])
        .output()
        .map_err(|e| format!("spawn sh alias: {e}"))?;
    if !out.status.success() {
        return Err(format!("sh alias script failed: {:?}", out.status));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "n=2 one=alpha two=beta" {
        return Err(format!("sh alias args: got {stdout:?}"));
    }
    Ok(())
}

/// trap on EXIT — configure scripts install cleanup traps.
#[test]
fn trap_on_exit_fires() -> Result<(), String> {
    let out = stdout_of("trap 'echo cleaned' EXIT; echo body")?;
    if out.trim() != "body\ncleaned" {
        return Err(format!("trap EXIT: got {out:?}"));
    }
    Ok(())
}
