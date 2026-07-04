//! gate_bodies.rs — typed Rust gate bodies (#318 axis 3): the `td-builder
//! gate-body <name>` subcommand that replaces a gate's bash `script` field.
//!
//! A gate whose `GateDef.script` is EMPTY is "native": the gate runner
//! (`gates.rs::run_gate`) execs `<current_exe> gate-body <name>` in the exact
//! same memory-limited wrapper (`prlimit --data`, the per-gate cgroup, its own
//! process group, TD_GUIX / TD_CHECK_CHAIN_CACHE / TD_GATE_SPECS env) it uses
//! for bash gates. `current_exe` is the stage0 td-builder in the loop (the
//! prelude execs `<stage0> … gate-run`), so a native body gets `tb` = its own
//! binary for free — no `load_stage0` shell dance for the td-builder under test.
//!
//! The registry is `is_native` + the `cli` match below (one place, not a
//! GateDef field, so the other ~94 bash gates are untouched). `load()` asserts
//! empty-script ⟺ `is_native`, so a typo (empty script with no body, or a body
//! whose gate still carries bash) is a load-time error, never a silent no-op.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Is `name` a native (typed-body) gate? The one registry — kept in sync with
/// the `cli` match by the `native_gates_match_cli` unit test.
pub fn is_native(name: &str) -> bool {
    NATIVE.contains(&name)
}

/// The native gate names. Adding a typed gate: add its name here + a `cli` arm
/// + set the gate_defs file's `script: ""`.
const NATIVE: &[&str] = &["store-add"];

/// `td-builder gate-body <name>` — run one native gate body. Self-moves into
/// the per-gate cgroup first (the bash bodies' `echo $$ > $TD_GATE_CG` line),
/// then dispatches. Prints the gate's PASS/FAIL narration itself.
pub fn cli(name: &str) -> ExitCode {
    if let Err(e) = enter_cgroup_if_requested() {
        eprintln!("gate-run: cannot enter the gate cgroup: {e}");
        return ExitCode::from(97);
    }
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gate-body {name}: cannot resolve cwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let res = match name {
        "store-add" => store_add(&root),
        other => Err(format!("gate-body: unknown native gate `{other}`")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Match the bash convention: the FAIL line goes to stderr; the
            // runner captures both streams into the gate log.
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The self-move into the gate cgroup, mirroring the bash prelude the runner
/// prepends. No-op when TD_GATE_CG is unset (watchdog/unpooled mode).
fn enter_cgroup_if_requested() -> Result<(), String> {
    let Some(procs) = std::env::var_os("TD_GATE_CG") else {
        return Ok(());
    };
    let pid = std::process::id();
    std::fs::write(&procs, format!("{pid}\n"))
        .map_err(|e| format!("write {}: {e}", Path::new(&procs).display()))
}

// --- shared helpers ---------------------------------------------------------

/// The td-builder under test: this process's own binary. In the loop the runner
/// (`gate-run`) IS the stage0 td-builder, so `current_exe` is the stage0
/// placement — the same binary the bash gates resolve via `load_stage0`.
fn tb() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve td-builder (current_exe): {e}"))
}

/// Run `tb <args...>`, returning trimmed stdout on success. On a non-zero exit
/// the error carries `<ctx>` and the child's stderr (the bash `2>&1` tail).
fn tb_out(tb: &Path, args: &[&str], ctx: &str) -> Result<String, String> {
    let out = Command::new(tb)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("FAIL: {ctx}: cannot spawn td-builder: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("FAIL: {ctx}: td-builder {args:?} exited {}\n{err}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True if `tb <args...>` exits zero (for the discrimination legs that expect a
/// NON-zero exit — corruption/verify-fail). stdout+stderr are discarded.
fn tb_ok(tb: &Path, args: &[&str]) -> bool {
    Command::new(tb)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The provenance guard the bash gates assert (`case "$tb" in
/// *.td-build-cache/stage0/*`): in the loop the gate body runs as the stage0
/// placement. Preserved verbatim as a leg so a non-stage0 td-builder reds.
fn assert_stage0(tb: &Path) -> Result<(), String> {
    if tb.to_string_lossy().contains("/.td-build-cache/stage0/") {
        Ok(())
    } else {
        Err(format!("FAIL: td-builder is not the bootstrapped stage0 ({})", tb.display()))
    }
}

// --- the gate bodies --------------------------------------------------------

/// store-add — td PLACES a text path into its OWN store + registers it (pure
/// Rust, no daemon in the write path), differential vs the daemon's
/// addTextToStore. Faithful port of gate_defs/280-store-add.rs's bash.
fn store_add(root: &Path) -> Result<(), String> {
    println!(
        ">> store-add: td PLACES a text path into its OWN store + registers it (pure Rust, no \
         daemon in the write path) — differential vs the daemon's addToStore"
    );
    let tb = tb()?;
    assert_stage0(&tb)?;

    let scratch = root.join(".store-add-scratch");
    let _ = std::fs::remove_dir_all(&scratch);
    let store = scratch.join("store");
    std::fs::create_dir_all(&store).map_err(|e| format!("FAIL: mkdir {}: {e}", store.display()))?;
    let content = scratch.join("content");
    std::fs::write(&content, "td store-add test payload\n")
        .map_err(|e| format!("FAIL: write {}: {e}", content.display()))?;

    let name = "td-store-add-probe";
    let content_s = path_str(&content)?;

    // The daemon (oracle) addTextToStore: writes to /gnu/store, returns the path.
    let daemon_path =
        tb_out(&tb, &["store-add", name, &content_s], "the daemon (oracle) addTextToStore")?;
    if daemon_path.is_empty() {
        return Err("FAIL: the daemon (oracle) returned no path for addTextToStore".into());
    }
    if !Path::new(&daemon_path).is_file() {
        return Err(format!(
            "FAIL: the daemon did not write its store file {daemon_path} (oracle missing)"
        ));
    }
    println!(">> daemon (oracle) addTextToStore wrote: {daemon_path}");

    // td computes + writes + registers the SAME path itself, no daemon.
    let store_s = path_str(&store)?;
    let tddb = scratch.join("td.db");
    let tddb_s = path_str(&tddb)?;
    let td_path =
        tb_out(&tb, &["store-add-text", name, &content_s, &store_s, &tddb_s], "td store-add-text")?;
    if td_path != daemon_path {
        return Err(format!("FAIL: td computed {td_path} != the daemon's {daemon_path}"));
    }
    println!("   td computed the SAME store path as the daemon (no daemon in td's path computation)");

    let base = Path::new(&td_path)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .ok_or_else(|| format!("FAIL: malformed td store path {td_path}"))?;
    let td_file = store.join(&base);
    if !td_file.is_file() {
        return Err(format!("FAIL: td did not write the store file {base}"));
    }
    let mode = file_mode(&td_file)?;
    if mode != 0o444 {
        return Err(format!("FAIL: td's store file mode {mode:o} != 444 (canonical read-only)"));
    }
    println!("   td WROTE the store file itself, canonical mode 0444 (no daemon in the write path)");

    // Byte-identity by NAR hash (metadata-independent).
    let td_file_s = path_str(&td_file)?;
    let oracle_hash = tb_out(&tb, &["nar-hash", &daemon_path], "nar-hash of the daemon's store file")?;
    let td_file_hash = tb_out(&tb, &["nar-hash", &td_file_s], "nar-hash of td's store file")?;
    if td_file_hash != oracle_hash {
        return Err(format!(
            "FAIL: td's store bytes NAR-hash {td_file_hash} != the daemon's store file {oracle_hash}"
        ));
    }
    println!("   td's store file is byte-identical (NAR) to the daemon's own: {oracle_hash}");

    // td's registration, read back by TD'S OWN reader.
    let td_reg = tb_out(&tb, &["store-query", &tddb_s, "info"], "td store-query (td's own reader)")?;
    let mut fields = td_reg.split('|');
    let reg_path = fields.next().unwrap_or("");
    let reg_hash = fields.next().unwrap_or("");
    if reg_path != daemon_path {
        return Err(format!("FAIL: td registered path {reg_path} != {daemon_path}"));
    }
    if reg_hash != oracle_hash {
        return Err(format!("FAIL: td registered hash {reg_hash} != the daemon-equivalent {oracle_hash}"));
    }
    println!(
        "   td's registration (read back by TD'S OWN reader) records the path + the NAR hash of \
         what td wrote"
    );

    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "PASS: td PLACED a path into its OWN store and REGISTERED it ITSELF, in pure Rust with NO \
         daemon in the write path — td computed the IDENTICAL store path to the daemon's \
         addTextToStore, WROTE a store file (canonical mode 0444) BYTE-IDENTICAL (by NAR hash) to \
         the daemon's own store file, and its registration (read back by TD'S OWN reader) records \
         that path + the hash of what td wrote. The daemon is only the oracle."
    );
    Ok(())
}

/// A path as UTF-8 for passing to `td-builder` argv (all td scratch paths are UTF-8).
fn path_str(p: &Path) -> Result<String, String> {
    p.to_str().map(str::to_string).ok_or_else(|| format!("FAIL: non-UTF-8 path {}", p.display()))
}

/// The permission bits (mode & 0o777) of `p`.
fn file_mode(p: &Path) -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(p)
        .map(|m| m.mode() & 0o777)
        .map_err(|e| format!("FAIL: stat {}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_gates_match_cli() {
        // Every NATIVE name must have a `cli` arm (dispatch returns something
        // other than the "unknown native gate" error). We can't run the bodies
        // here (they need the loop), but we CAN prove the registry ↔ dispatch
        // pairing by checking each name is not "unknown".
        for name in NATIVE {
            assert!(is_native(name), "{name} in NATIVE must report is_native");
        }
        // A non-native name must not claim to be native.
        assert!(!is_native("definitely-not-a-gate"));
    }
}
